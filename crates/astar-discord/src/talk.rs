// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Who is talking, inferred from packet TIMING alone.
//!
//! Discord has no "stopped speaking" event. Its speaking opcode says a user
//! HAS started — it is sent the first time they transmit, and again only
//! when their capabilities change — and it never says they stopped. So the
//! only way to learn that somebody has stopped is that their packets
//! stopped arriving.
//! songbird's driver reports that as a 20 ms `VoiceTick` carrying two sets
//! of SSRCs: the ones that produced a packet for this tick, and the ones it
//! knows about that did not.
//!
//! **No audio reaches this file, and none could.** The driver runs in
//! `DecodeMode::Pass` (see `gateway::voice_config`), so nothing is decrypted
//! and nothing is decoded; and what crosses into here is a set of 32-bit
//! numbers, never a packet. Everything below is ordinary integer
//! bookkeeping, which is why it is compiled and tested with no Discord in
//! the build at all.
//!
//! Two pieces, deliberately separate:
//!
//! * [`TalkEdges`] turns a stream of ticks into edges — started, stopped —
//!   with the falling edge debounced, because a talker's packets are not a
//!   solid wall. Jitter, a lost packet and the client's own discontinuous
//!   transmission all produce short gaps that are not the end of a
//!   sentence.
//! * [`Talkers`] puts the SSRC → user mapping on top, because an edge for
//!   an SSRC nobody can name is not something the node can act on — and
//!   aggregates per PERSON, because one person can be behind two SSRCs at
//!   once and only the person is something the node can act on.

use std::collections::{BTreeSet, HashMap, HashSet};

/// How many consecutive ticks without a packet end a talk spurt: ten ticks
/// at 20 ms is **200 ms**.
///
/// The falling edge is the one that needs a debounce and the rising edge is
/// not: a gap is ambiguous and the start of speech is not. 200 ms is long
/// enough to ride out ordinary jitter, a couple of lost packets, and the
/// pauses a discontinuous-transmission client leaves between words; it is
/// short enough to be invisible next to the five-second silence window it
/// feeds, which is the only thing that acts on these edges.
const FALL_TICKS: u32 = 10;

/// Rising and falling edges from the tick stream, per SSRC.
///
/// Hearing a packet is immediate evidence of speech; hearing none is not
/// evidence of silence until it has gone on for [`FALL_TICKS`].
#[derive(Debug, Default)]
pub(crate) struct TalkEdges {
    /// Every SSRC currently considered talking, and how many consecutive
    /// ticks it has now been missing from the speaking set. Presence in
    /// this map IS the "talking" state; zero means it spoke this tick.
    talking: HashMap<u32, u32>,
}

impl TalkEdges {
    /// Fold one tick in, and report the edges it caused.
    ///
    /// `speaking` is the set of SSRCs that produced a packet for this tick;
    /// `silent` is the set songbird still knows about that did not. An SSRC
    /// in NEITHER set has left the call (or its decoder state has been
    /// pruned), and is treated exactly like a silent one: it stops being
    /// heard, which is the same fact arriving by a different route. Both
    /// therefore fall after the same [`FALL_TICKS`], and that is deliberate
    /// — somebody whose client crashed mid-word must release the mic on the
    /// same schedule as somebody who simply stopped.
    ///
    /// The returned edges are sorted by SSRC so that a tick which moves two
    /// talkers at once produces the same vector every time; hash iteration
    /// order is not something a caller — or a test — should have to think
    /// about.
    pub(crate) fn observe(
        &mut self,
        speaking: &HashSet<u32>,
        silent: &HashSet<u32>,
    ) -> Vec<(u32, bool)> {
        // songbird documents `silent` as "all SSRCs currently known in the
        // call who aren't included in speaking", so the two sets are
        // disjoint. Nothing below depends on that, but a version that
        // stopped being true would mean the tick no longer says what this
        // file thinks it says.
        debug_assert!(
            speaking.is_disjoint(silent),
            "songbird's silent set excludes the speaking SSRCs"
        );

        let mut edges = Vec::new();

        for &ssrc in speaking {
            match self.talking.entry(ssrc) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(0);
                    edges.push((ssrc, true));
                }
                // Already talking: the miss counter goes back to zero, which
                // is what makes a gap have to be CONSECUTIVE to count.
                std::collections::hash_map::Entry::Occupied(mut slot) => *slot.get_mut() = 0,
            }
        }

        self.talking.retain(|ssrc, missed| {
            if speaking.contains(ssrc) {
                return true;
            }
            *missed += 1;
            if *missed >= FALL_TICKS {
                edges.push((*ssrc, false));
                false
            } else {
                true
            }
        });

        edges.sort_unstable();
        edges
    }

    /// Whether this SSRC is currently considered to be talking.
    fn is_talking(&self, ssrc: u32) -> bool {
        self.talking.contains_key(&ssrc)
    }

    /// Forget everything: no SSRC is talking, and no SSRC is part-way
    /// through a debounce.
    ///
    /// For the one case where the counters are not evidence about anybody
    /// any more — the voice connection dropped, so the packets that would
    /// have continued a spurt cannot arrive and their absence says nothing.
    /// See [`Talkers::interrupt`].
    fn forget(&mut self) {
        self.talking.clear();
    }
}

/// [`TalkEdges`] plus the SSRC → user mapping, which is the form the rest of
/// the node can use.
///
/// **A talker is a person, not a stream id.** One Discord user can be behind
/// more than one SSRC at once — Discord issues a new one when a client
/// reconnects and never withdraws the old one, and nothing in the protocol
/// says the two belong together except the speaking opcode that names them
/// both. So a user's state here is the OR across their SSRCs, and an edge is
/// reported only when that AGGREGATE moves. Translating each SSRC edge on
/// its own instead produced two rising edges for one person, and — the
/// failure that matters — reported the old stream's LATE falling edge while
/// the new one was mid-sentence: the mic holder was marked silent while
/// speaking, and the five-second silence window unkeyed them mid-over.
///
/// **An edge for an SSRC nobody can name is held back, not queued.** The
/// mapping arrives in Discord's speaking opcode, and that opcode is not
/// guaranteed to arrive before the packets it describes — so the first
/// moments of a talk spurt can be unattributable. Replaying the held-back
/// edge later would announce a start that is already over.
///
/// What happens instead is that the mapping is treated as a fresh
/// observation of the CURRENT state: when an SSRC is named and its user's
/// aggregate changes as a result, the edge is emitted then, at mapping time.
/// That is the case that matters, and getting it wrong is not cosmetic —
/// the mic holder who was already mid-sentence when their SSRC was named
/// would never be reported talking at all, and the silence timer would
/// unkey them five seconds into their over.
#[derive(Debug, Default)]
pub(crate) struct Talkers {
    edges: TalkEdges,
    /// SSRC → Discord user id, learned from speaking-state updates.
    ///
    /// Pruned in [`Self::tick`] when an SSRC falls AND songbird has stopped
    /// listing it. Without that, this map grew by one entry per reconnect
    /// for the life of a session, and every stale entry was another way for
    /// one person to be two talkers.
    ///
    /// **It is not a complete prune, and that is the safe direction.** An
    /// SSRC that goes quiet falls while it is still listed in `silent` —
    /// correctly, because its user is still in the channel and their name
    /// must survive until they speak again — and songbird then expires the
    /// decoder state 60 s later (`Config::decode_state_timeout`,
    /// `songbird-0.6.0/src/config.rs:215`, swept by the `retain` at
    /// `src/driver/tasks/udp_rx/mod.rs:145`). There is no second falling
    /// edge at that point, so nothing prunes the entry: somebody who talks
    /// once and then leaves quietly stays in this map for the session.
    ///
    /// The cost is bounded by the number of distinct SSRCs one session
    /// sees, which is bounded by the people in the channel and their
    /// reconnects — kilobytes, not a leak with a shape. The stale entry
    /// cannot mis-attribute anything either: its SSRC produces no packets,
    /// so it contributes nothing to the aggregate, and if Discord ever
    /// re-issued that number the speaking opcode naming its new owner
    /// arrives with the packets and overwrites the entry. Pruning harder
    /// would mean dropping names that are still in use, which is the
    /// failure that actually hurts — a mic holder reported silent while
    /// they speak.
    users: HashMap<u32, u64>,
    /// The users last REPORTED talking — the state the node believes.
    ///
    /// Every emission is a diff against this set, which is what makes the
    /// edges idempotent: a tick that does not move the aggregate says
    /// nothing, whatever it did to the SSRCs underneath. A `BTreeSet` so
    /// that a batch is sorted and unique by construction rather than by a
    /// sort the caller has to remember.
    talking: BTreeSet<u64>,
    /// Set by [`Self::end`]. A session's handlers live on songbird's `Call`
    /// and this end of the wire cannot unregister them from the node's
    /// thread, so a stale handler can still deliver a tick after the
    /// session it belongs to is over. Once ended, this reports nothing
    /// further: the Commander has already been told that everyone stopped.
    ended: bool,
}

impl Talkers {
    /// Record `ssrc` as belonging to `user`, and report the edges that
    /// mapping reveals.
    ///
    /// Three things can happen, and all three come out of the same diff:
    ///
    /// * a name for a stream that is talking, for a user who was not —
    ///   `(user, true)`;
    /// * a name for a stream that is talking, for a user who already was
    ///   (their other stream) — nothing, because nothing about the person
    ///   changed;
    /// * a stream RE-POINTED at a different user while it is talking — the
    ///   old user falls if that was their last live stream, and the new one
    ///   rises. Discord does not re-point an SSRC in practice, but the map
    ///   cannot express "this used to be someone else's", so the arithmetic
    ///   has to be right rather than the case impossible.
    ///
    /// A repeat of a mapping already held emits nothing without looking
    /// further. Repeats really do happen — the opcode is re-sent when a
    /// client's capabilities change, which is the one case that brings it
    /// back after the first transmission (see this module's docs) — and a
    /// re-send is not a new spurt.
    pub(crate) fn map_ssrc(&mut self, ssrc: u32, user: u64) -> Vec<(u64, bool)> {
        if self.ended {
            return Vec::new();
        }
        if self.users.insert(ssrc, user) == Some(user) {
            return Vec::new();
        }
        self.settle()
    }

    /// Fold one tick in, and report the edges for the users behind the
    /// SSRCs that moved.
    pub(crate) fn tick(
        &mut self,
        speaking: &HashSet<u32>,
        silent: &HashSet<u32>,
    ) -> Vec<(u64, bool)> {
        if self.ended {
            return Vec::new();
        }
        let edges = self.edges.observe(speaking, silent);
        for (ssrc, talking) in &edges {
            // Retired on both counts: [`TalkEdges`] has just dropped it
            // (that is what a falling edge IS — see `observe`'s `retain`),
            // and songbird no longer lists it in either set, which means
            // its decoder state has been pruned and the number is free to
            // be handed to somebody else. Keeping the mapping would credit
            // a stranger's stream to whoever held that number last.
            //
            // The `speaking`/`silent` half of the condition is load-bearing
            // in the other direction too: an SSRC that merely went QUIET is
            // still listed in `silent`, its user is still in the channel,
            // and their name must survive until they speak again. And a
            // mapping for an SSRC that has never been heard at all is
            // listed in neither set — songbird only learns an SSRC from a
            // packet — so it never has a falling edge and is never pruned.
            if !*talking && !speaking.contains(ssrc) && !silent.contains(ssrc) {
                self.users.remove(ssrc);
            }
        }
        self.settle()
    }

    /// The voice connection dropped under this session: report everyone
    /// still marked talking as stopped, forget every stream, and stay
    /// usable.
    ///
    /// **Not an [`Self::end`].** songbird retries a dropped driver on its
    /// own (`Config::driver_retry`), so the session is expected to come
    /// back; what must not survive the gap is the belief that somebody is
    /// mid-word, because the packets that would have ended their spurt
    /// cannot arrive and their absence therefore proves nothing.
    ///
    /// **The mappings go too**, and that is not tidiness: a reconnect
    /// builds a fresh `udp_rx` task with an empty decoder map and a fresh
    /// SSRC tracker (songbird 0.6.0 `src/driver/connection/mod.rs:241,265`),
    /// so every SSRC is issued again and every mapping is re-announced by a
    /// fresh speaking opcode. An SSRC carried across the gap is a number
    /// that now means somebody else.
    pub(crate) fn interrupt(&mut self) -> Vec<u64> {
        if self.ended {
            return Vec::new();
        }
        self.edges.forget();
        self.users.clear();
        self.stop_everyone()
    }

    /// End the session: report everyone currently talking so the caller can
    /// say they stopped, and stop reporting anything afterwards.
    ///
    /// The Commander's hard time-out timer would eventually rescue a holder
    /// left marked talking, but the backstop is for the abnormal path. A
    /// leave is the normal path, and on it the node must not be left
    /// believing somebody is mid-word.
    pub(crate) fn end(&mut self) -> Vec<u64> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        self.stop_everyone()
    }

    /// Take the reported-talking set and hand it back, sorted and unique.
    /// Shared by the two ways a session stops reporting.
    fn stop_everyone(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.talking).into_iter().collect()
    }

    /// Who is talking RIGHT NOW, by the OR rule: a user is talking when any
    /// SSRC mapped to them is.
    fn aggregate(&self) -> BTreeSet<u64> {
        self.users
            .iter()
            .filter(|(ssrc, _)| self.edges.is_talking(**ssrc))
            .map(|(_, user)| *user)
            .collect()
    }

    /// Diff the truth against what the node has been told, report the
    /// difference, and remember the new truth.
    ///
    /// The one place an edge is emitted, which is why every caller above is
    /// three lines long. Output is sorted ascending by user id with a stop
    /// before a start for the same id (`false < true`), and cannot contain
    /// a duplicate: it is built from two set differences.
    fn settle(&mut self) -> Vec<(u64, bool)> {
        let now = self.aggregate();
        let mut edges: Vec<(u64, bool)> = self
            .talking
            .difference(&now)
            .map(|&user| (user, false))
            .chain(now.difference(&self.talking).map(|&user| (user, true)))
            .collect();
        self.talking = now;
        edges.sort_unstable();
        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tick where `speaking` holds these SSRCs and `silent` holds the
    /// rest of `known`.
    fn tick(edges: &mut TalkEdges, speaking: &[u32], silent: &[u32]) -> Vec<(u32, bool)> {
        let speaking: HashSet<u32> = speaking.iter().copied().collect();
        let silent: HashSet<u32> = silent.iter().copied().collect();
        edges.observe(&speaking, &silent)
    }

    #[test]
    fn the_first_tick_an_ssrc_speaks_is_a_rising_edge() {
        // Catches a rising edge that waits for confirmation: the mic holder
        // must be reported talking the moment a packet arrives, because the
        // silence timer is already running from key-up.
        let mut e = TalkEdges::default();
        assert_eq!(tick(&mut e, &[7], &[]), vec![(7, true)]);
    }

    #[test]
    fn continuous_speech_produces_exactly_one_rising_edge() {
        // Catches an `observe` that re-announces on every tick: fifty
        // `Talking{true}` a second would be correct in meaning and useless
        // in practice.
        let mut e = TalkEdges::default();
        assert_eq!(tick(&mut e, &[7], &[]), vec![(7, true)]);
        for _ in 0..50 {
            assert_eq!(tick(&mut e, &[7], &[]), vec![], "still the same spurt");
        }
    }

    #[test]
    fn the_falling_edge_lands_on_the_tenth_silent_tick_and_not_the_ninth() {
        // Catches both an off-by-one (`>` instead of `>=`, or the counter
        // incremented after the test) and a debounce that was quietly
        // shortened. Nine ticks of nothing is still the same over.
        let mut e = TalkEdges::default();
        assert_eq!(tick(&mut e, &[7], &[]), vec![(7, true)]);
        for n in 1..FALL_TICKS {
            assert_eq!(
                tick(&mut e, &[], &[7]),
                vec![],
                "silent tick {n} is too early"
            );
        }
        assert_eq!(tick(&mut e, &[], &[7]), vec![(7, false)]);
    }

    #[test]
    fn a_short_gap_produces_nothing_and_resets_the_counter() {
        // The whole point of the debounce. Three ticks of jitter must not
        // chatter — and, the half that is easy to get wrong, the counter
        // must go back to ZERO when speech resumes. A counter that merely
        // stopped growing would fire on the seventh tick of the next gap.
        let mut e = TalkEdges::default();
        assert_eq!(tick(&mut e, &[7], &[]), vec![(7, true)]);
        for _ in 0..3 {
            assert_eq!(tick(&mut e, &[], &[7]), vec![]);
        }
        assert_eq!(tick(&mut e, &[7], &[]), vec![], "already talking");
        for n in 1..FALL_TICKS {
            assert_eq!(
                tick(&mut e, &[], &[7]),
                vec![],
                "the counter restarted, so silent tick {n} is too early"
            );
        }
        assert_eq!(tick(&mut e, &[], &[7]), vec![(7, false)]);
    }

    #[test]
    fn two_ssrcs_keep_their_own_counters() {
        // Catches a single shared counter, which is the obvious way to get
        // this wrong: in a channel with two talkers, one of them speaking
        // would hold the other's mic open forever.
        let mut e = TalkEdges::default();
        assert_eq!(tick(&mut e, &[7, 9], &[]), vec![(7, true), (9, true)]);
        // 9 keeps talking; 7 goes quiet and must fall on its own.
        for _ in 1..FALL_TICKS {
            assert_eq!(tick(&mut e, &[9], &[7]), vec![]);
        }
        assert_eq!(tick(&mut e, &[9], &[7]), vec![(7, false)]);
        for _ in 1..FALL_TICKS {
            assert_eq!(tick(&mut e, &[], &[9]), vec![]);
        }
        assert_eq!(tick(&mut e, &[], &[9]), vec![(9, false)]);
    }

    #[test]
    fn an_ssrc_that_vanishes_from_both_sets_falls_on_the_same_schedule() {
        // Somebody whose client crashed or who left the channel mid-word:
        // songbird stops listing the SSRC at all rather than moving it to
        // `silent`. Catches an implementation that only ever counts SSRCs
        // it can still see in `silent`, which would leave that talker
        // marked talking forever and the mic open until the hard timer.
        let mut e = TalkEdges::default();
        assert_eq!(tick(&mut e, &[7], &[]), vec![(7, true)]);
        for _ in 1..FALL_TICKS {
            assert_eq!(tick(&mut e, &[], &[]), vec![]);
        }
        assert_eq!(tick(&mut e, &[], &[]), vec![(7, false)]);
    }

    #[test]
    fn an_ssrc_that_was_never_speaking_produces_nothing() {
        // Everyone else in a busy channel is in `silent` on every tick.
        // Catches an implementation that treats "known and quiet" as a
        // state to fall out of, which would emit a `false` edge per member
        // per 200 ms.
        let mut e = TalkEdges::default();
        for _ in 0..(FALL_TICKS * 3) {
            assert_eq!(tick(&mut e, &[], &[1, 2, 3]), vec![]);
        }
    }

    // -- the mapping ------------------------------------------------------

    #[test]
    fn edges_for_an_unmapped_ssrc_are_held_back() {
        let mut t = Talkers::default();
        let speaking: HashSet<u32> = [7].into_iter().collect();
        let silent = HashSet::new();
        assert_eq!(
            t.tick(&speaking, &silent),
            vec![],
            "nobody to attribute it to"
        );
    }

    #[test]
    fn a_mapping_that_arrives_mid_spurt_reports_the_talker_immediately() {
        // THE case this design exists for. The speaking opcode can arrive
        // after the packets it describes, so the holder may already be
        // mid-sentence when their SSRC is finally named. Emitting nothing
        // here — the obvious "edges only" implementation — means the node
        // never hears that they are talking, and the five-second silence
        // window unkeys them five seconds into their over.
        let mut t = Talkers::default();
        let speaking: HashSet<u32> = [7].into_iter().collect();
        let silent = HashSet::new();
        assert_eq!(t.tick(&speaking, &silent), vec![]);
        assert_eq!(t.map_ssrc(7, 300), vec![(300, true)]);
        // And it is not repeated on the next tick, nor on a repeat of the
        // same mapping: the spurt already started.
        assert_eq!(t.tick(&speaking, &silent), vec![]);
        assert_eq!(t.map_ssrc(7, 300), vec![]);
    }

    #[test]
    fn a_mapping_for_a_quiet_ssrc_reports_nothing() {
        // The ordinary case: the opcode arrives first. Catches a `map_ssrc`
        // that announces a talker on every mapping, which would key the
        // silence logic for somebody who has said nothing.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
    }

    #[test]
    fn a_mapped_ssrc_reports_both_edges_against_its_user() {
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        let speaking: HashSet<u32> = [7].into_iter().collect();
        let silent: HashSet<u32> = [7].into_iter().collect();
        let none = HashSet::new();
        assert_eq!(t.tick(&speaking, &none), vec![(300, true)]);
        for _ in 1..FALL_TICKS {
            assert_eq!(t.tick(&none, &silent), vec![]);
        }
        assert_eq!(t.tick(&none, &silent), vec![(300, false)]);
    }

    #[test]
    fn ending_a_session_reports_whoever_was_still_talking_exactly_once() {
        // The Commander must never be left believing a holder is mid-word
        // after the bot has left. Catches an `end` that returns nothing,
        // and — via the second call — one that could be run twice by the
        // leave path and the teardown path both.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(t.map_ssrc(9, 500), vec![]);
        let speaking: HashSet<u32> = [7].into_iter().collect();
        assert_eq!(t.tick(&speaking, &HashSet::new()), vec![(300, true)]);

        assert_eq!(t.end(), vec![300], "only the one who was talking");
        assert_eq!(t.end(), Vec::<u64>::new(), "and only once");
    }

    #[test]
    fn an_ended_session_reports_nothing_further() {
        // A stale handler on songbird's `Call` can still deliver a tick
        // after the session is over. Catches a `Talkers` that would answer
        // it, which would resurrect a holder the node has already been told
        // stopped.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(t.end(), Vec::<u64>::new());
        let speaking: HashSet<u32> = [7].into_iter().collect();
        assert_eq!(t.tick(&speaking, &HashSet::new()), vec![]);
        assert_eq!(t.map_ssrc(9, 500), vec![]);
    }

    // -- one user, several stream ids ------------------------------------
    //
    // Everything below exists because `users` is a map from SSRC to user
    // and Discord hands out a NEW SSRC when a client reconnects. The old
    // mapping is not withdrawn — there is no opcode that withdraws one —
    // so for a while two SSRCs name the same person, and a rule that
    // answered per SSRC answered for a stream rather than for a human.

    /// A tick where `speaking` holds these SSRCs and `silent` holds the
    /// rest, as [`Talkers`] sees it.
    fn utick(t: &mut Talkers, speaking: &[u32], silent: &[u32]) -> Vec<(u64, bool)> {
        let speaking: HashSet<u32> = speaking.iter().copied().collect();
        let silent: HashSet<u32> = silent.iter().copied().collect();
        t.tick(&speaking, &silent)
    }

    #[test]
    fn two_ssrcs_for_one_user_are_one_talker() {
        // Catches the per-SSRC mapping: it emitted `[(300, true), (300,
        // true)]` from one tick, because each edge was translated on its
        // own and nothing asked whether the USER's state had changed.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(t.map_ssrc(9, 300), vec![]);
        assert_eq!(utick(&mut t, &[7, 9], &[]), vec![(300, true)]);
    }

    #[test]
    fn one_stream_falling_while_another_speaks_is_not_a_stop() {
        // THE bug. The old mapping fell ten ticks after its last packet
        // while the new one was mid-sentence, and `(300, false)` went to
        // the Commander for somebody who was still talking — which starts
        // the five-second silence clock and unkeys them mid-over.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(t.map_ssrc(9, 300), vec![]);
        assert_eq!(utick(&mut t, &[7, 9], &[]), vec![(300, true)]);
        for n in 1..=FALL_TICKS {
            assert_eq!(
                utick(&mut t, &[9], &[7]),
                vec![],
                "9 is still speaking on silent tick {n}, so 300 has not stopped"
            );
        }
        assert_eq!(t.end(), vec![300], "and 300 is still talking at the end");
    }

    #[test]
    fn the_user_stops_on_the_tick_the_last_of_their_streams_falls() {
        // The other half: the aggregate really does fall, exactly once,
        // and exactly when the last stream goes. Catches an aggregate that
        // never falls at all (mic open until the 180 s hard timer) and one
        // that falls with the FIRST stream.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(t.map_ssrc(9, 300), vec![]);
        assert_eq!(utick(&mut t, &[7, 9], &[]), vec![(300, true)]);
        // 9 goes quiet first and falls on its own, silently.
        for n in 1..=FALL_TICKS {
            assert_eq!(
                utick(&mut t, &[7], &[9]),
                vec![],
                "7 still speaks, so silent tick {n} of 9 says nothing"
            );
        }
        // Now 7 goes too, and the last one carries the user down.
        for n in 1..FALL_TICKS {
            assert_eq!(
                utick(&mut t, &[], &[7, 9]),
                vec![],
                "silent tick {n} is inside the debounce"
            );
        }
        assert_eq!(utick(&mut t, &[], &[7, 9]), vec![(300, false)]);
        assert_eq!(
            utick(&mut t, &[], &[7, 9]),
            vec![],
            "and it is not repeated once they are down"
        );
    }

    #[test]
    fn a_new_stream_inside_the_old_ones_debounce_never_latches_the_user_silent() {
        // The reconnect, at 20 ms resolution. The client comes back with a
        // new SSRC and starts talking again while the old SSRC is still
        // inside its 200 ms debounce; the old one's LATE falling edge must
        // not be reported, because the person never stopped.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(utick(&mut t, &[7], &[]), vec![(300, true)]);

        // The reconnect: 7 is gone from songbird's view entirely, 11 is the
        // new stream and is heard before anyone has named it.
        assert_eq!(utick(&mut t, &[11], &[]), vec![], "11 has no user yet");
        assert_eq!(
            t.map_ssrc(11, 300),
            vec![],
            "300 is already talking through 7, so this is not a new spurt"
        );
        // Run well past the old stream's debounce with the new one live.
        let mut emitted = Vec::new();
        for _ in 0..(FALL_TICKS * 3) {
            emitted.extend(utick(&mut t, &[11], &[]));
        }
        assert_eq!(
            emitted,
            Vec::<(u64, bool)>::new(),
            "the old stream's falling edge must never reach the node"
        );
        assert_eq!(t.end(), vec![300], "and 300 ends the session talking");
    }

    #[test]
    fn re_pointing_a_stream_at_another_user_moves_the_talking_with_it() {
        // Ordering rule: ascending by user id, and `false` before `true`
        // for one id. 300 loses their only stream and 500 gains it, so one
        // tick's worth of mapping produces both edges, sorted.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(utick(&mut t, &[7], &[]), vec![(300, true)]);
        assert_eq!(t.map_ssrc(7, 500), vec![(300, false), (500, true)]);
        assert_eq!(t.end(), vec![500], "only the new owner is talking");
    }

    #[test]
    fn re_pointing_one_of_two_streams_leaves_the_first_user_talking() {
        // The same move, but 300 keeps a stream of their own. Catches an
        // implementation that reports the OLD user stopped because one of
        // their SSRCs changed hands.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(t.map_ssrc(9, 300), vec![]);
        assert_eq!(utick(&mut t, &[7, 9], &[]), vec![(300, true)]);
        assert_eq!(t.map_ssrc(9, 500), vec![(500, true)]);
        assert_eq!(t.end(), vec![300, 500]);
    }

    #[test]
    fn a_late_mapping_onto_a_talking_user_says_nothing() {
        // The speaking opcode for the second stream arrives after the
        // packets. The user is already reported talking, so there is
        // nothing to say — a duplicate rising edge would be harmless to
        // the rule engine and a lie to anything that counts edges.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(utick(&mut t, &[7, 9], &[]), vec![(300, true)]);
        assert_eq!(t.map_ssrc(9, 300), vec![]);
    }

    #[test]
    fn ending_reports_a_user_with_two_live_streams_exactly_once() {
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(t.map_ssrc(9, 300), vec![]);
        assert_eq!(t.map_ssrc(11, 500), vec![]);
        assert_eq!(
            utick(&mut t, &[7, 9, 11], &[]),
            vec![(300, true), (500, true)]
        );
        assert_eq!(t.end(), vec![300, 500]);
    }

    #[test]
    fn a_retired_stream_id_is_forgotten_and_not_reused_against_its_old_user() {
        // The map must not grow for the life of a session, and the number
        // that comes back must not be attributed to whoever held it last:
        // SSRCs are per-session and Discord reuses the space freely.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(utick(&mut t, &[7], &[]), vec![(300, true)]);
        for _ in 1..FALL_TICKS {
            assert_eq!(utick(&mut t, &[], &[]), vec![]);
        }
        assert_eq!(utick(&mut t, &[], &[]), vec![(300, false)]);

        // Same number, new session, nobody has named it yet.
        assert_eq!(
            utick(&mut t, &[7], &[]),
            vec![],
            "a number nobody has named belongs to nobody"
        );
        assert_eq!(t.end(), Vec::<u64>::new());
    }

    #[test]
    fn a_stream_that_goes_quiet_and_speaks_again_keeps_its_name() {
        // The other guard on the pruning: going quiet is not leaving.
        // Somebody who stops for a second between overs is still listed in
        // songbird's `silent` set, and their mapping must survive — a
        // prune that fired on the falling edge alone would leave the next
        // over nameless, held back forever, with the mic holder reported
        // silent while they speak.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(utick(&mut t, &[7], &[]), vec![(300, true)]);
        for _ in 1..FALL_TICKS {
            assert_eq!(utick(&mut t, &[], &[7]), vec![]);
        }
        assert_eq!(utick(&mut t, &[], &[7]), vec![(300, false)]);
        for _ in 0..(FALL_TICKS * 2) {
            assert_eq!(utick(&mut t, &[], &[7]), vec![]);
        }
        assert_eq!(
            utick(&mut t, &[7], &[]),
            vec![(300, true)],
            "the same person, back for another over"
        );
    }

    #[test]
    fn a_mapping_for_a_stream_nobody_has_heard_yet_is_kept() {
        // The guard on the pruning above, and it is not hypothetical:
        // songbird only knows an SSRC once a packet has carried it, so a
        // user named by the speaking opcode BEFORE their first packet is
        // in neither set for every tick until they talk. Pruning on
        // absence would throw that mapping away and hold back the rising
        // edge of the very spurt it was meant to name.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        for _ in 0..(FALL_TICKS * 5) {
            assert_eq!(utick(&mut t, &[], &[9]), vec![]);
        }
        assert_eq!(utick(&mut t, &[7], &[9]), vec![(300, true)]);
    }

    // -- the connection dropping under us --------------------------------

    #[test]
    fn an_interrupt_reports_every_talker_exactly_once() {
        // songbird's driver dropped: kicked from the channel, or the
        // network went. No `Leave` is coming, so this is the only thing
        // that stops the node believing these two are mid-word.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(t.map_ssrc(9, 300), vec![]);
        assert_eq!(t.map_ssrc(11, 500), vec![]);
        assert_eq!(
            utick(&mut t, &[7, 9, 11], &[]),
            vec![(300, true), (500, true)]
        );
        assert_eq!(t.interrupt(), vec![300, 500]);
        assert_eq!(
            t.interrupt(),
            Vec::<u64>::new(),
            "and a second disconnect has nobody left to report"
        );
    }

    #[test]
    fn an_interrupt_forgets_the_stream_ids_but_leaves_the_session_usable() {
        // A reconnect gets a fresh `udp_rx` runner with an empty decoder
        // map and a fresh SSRC tracker (songbird 0.6.0
        // `src/driver/connection/mod.rs:241,265`), so every SSRC is new and
        // every mapping is re-announced. Carrying the old ones across would
        // attribute a stranger's stream to the last person who held that
        // number.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(utick(&mut t, &[7], &[]), vec![(300, true)]);
        assert_eq!(t.interrupt(), vec![300]);

        assert_eq!(
            t.map_ssrc(7, 300),
            vec![],
            "the edge state went with the connection: nobody is talking"
        );
        // And the session still works: this is not an `end`.
        assert_eq!(utick(&mut t, &[9], &[]), vec![], "9 has no user yet");
        assert_eq!(t.map_ssrc(9, 400), vec![(400, true)]);
    }

    #[test]
    fn an_interrupt_before_any_mapping_is_re_announced_holds_the_tick_back() {
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(utick(&mut t, &[7], &[]), vec![(300, true)]);
        assert_eq!(t.interrupt(), vec![300]);
        assert_eq!(
            utick(&mut t, &[7], &[]),
            vec![],
            "7 must be nameless again until a fresh opcode names it"
        );
    }

    #[test]
    fn ending_after_an_interrupt_reports_nothing_further() {
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(utick(&mut t, &[7], &[]), vec![(300, true)]);
        assert_eq!(t.interrupt(), vec![300]);
        assert_eq!(
            t.end(),
            Vec::<u64>::new(),
            "the interrupt already said they stopped"
        );
    }

    #[test]
    fn an_interrupt_after_the_session_ended_reports_nothing() {
        // A deliberate leave ends the session and THEN disconnects the
        // driver — songbird fires `DriverDisconnect` for a requested
        // disconnect too (`src/driver/tasks/mod.rs:101-115`). That stale
        // event must not re-report what `end` has already reported.
        let mut t = Talkers::default();
        assert_eq!(t.map_ssrc(7, 300), vec![]);
        assert_eq!(utick(&mut t, &[7], &[]), vec![(300, true)]);
        assert_eq!(t.end(), vec![300]);
        assert_eq!(t.interrupt(), Vec::<u64>::new());
    }
}
