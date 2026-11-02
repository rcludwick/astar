// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! When to talk to the directory servers. Sans-IO: the caller passes the
//! time in and performs the job that comes back.

use std::time::{Duration, Instant};

use crate::Timings;

/// What the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryJob {
    Login,
    StatusRefresh,
    ListRefresh,
}

/// The directory schedule: login, a status refresh every `status_refresh`,
/// a list refresh every `list_refresh` (or on demand, at most once per
/// `list_min_interval`), and exponential backoff across servers on error.
///
/// A refusal or bad password is final, not a transport error: the caller
/// reports it with [`DirectorySchedule::halt`], which stops all jobs until
/// [`DirectorySchedule::resume`]. Only [`DirectorySchedule::on_error`]
/// backs off and retries.
#[derive(Debug, Clone)]
pub struct DirectorySchedule {
    timings: Timings,
    server_count: usize,
    server: usize,
    needs_login: bool,
    login_at: Instant,
    status_at: Instant,
    list_at: Instant,
    last_list: Option<Instant>,
    backoff: Option<Duration>,
    in_flight: bool,
    halted: bool,
}

impl DirectorySchedule {
    /// A schedule whose first job, a login, is due at `now`.
    #[must_use]
    pub fn new(timings: Timings, server_count: usize, now: Instant) -> Self {
        Self {
            timings,
            server_count: server_count.max(1),
            server: 0,
            needs_login: true,
            login_at: now,
            status_at: now,
            list_at: now,
            last_list: None,
            backoff: None,
            in_flight: false,
            halted: false,
        }
    }

    /// The earliest instant a job comes due by the clock alone. It ignores
    /// `halt` and in-flight state, so to sleep until the next job use
    /// [`Self::wake_at`], which is `None` when nothing will come due.
    #[must_use]
    pub fn next_due(&self) -> Instant {
        if self.needs_login {
            self.login_at
        } else {
            self.status_at.min(self.list_at)
        }
    }

    /// When to call [`Self::due`] next: `None` while halted or while a job
    /// is in flight (those end only through a call, not the clock).
    #[must_use]
    pub fn wake_at(&self) -> Option<Instant> {
        (!self.halted && !self.in_flight).then(|| self.next_due())
    }

    /// The job to run now, if any. It stays in flight (no further job is
    /// returned) until `on_success`, `on_error` or `halt`.
    pub fn due(&mut self, now: Instant) -> Option<DirectoryJob> {
        if self.halted || self.in_flight {
            return None;
        }
        let job = if self.needs_login {
            (now >= self.login_at).then_some(DirectoryJob::Login)
        } else if now >= self.status_at && self.status_at <= self.list_at {
            Some(DirectoryJob::StatusRefresh)
        } else if now >= self.list_at {
            Some(DirectoryJob::ListRefresh)
        } else if now >= self.status_at {
            Some(DirectoryJob::StatusRefresh)
        } else {
            None
        };
        if job.is_some() {
            self.in_flight = true;
        }
        job
    }

    /// The job finished well; the backoff resets.
    pub fn on_success(&mut self, job: DirectoryJob, now: Instant) {
        self.in_flight = false;
        self.backoff = None;
        match job {
            DirectoryJob::Login => {
                self.needs_login = false;
                self.status_at = now + self.timings.status_refresh;
                // The first list comes straight after the login.
                self.list_at = now;
            }
            DirectoryJob::StatusRefresh => {
                self.status_at = now + self.timings.status_refresh;
            }
            DirectoryJob::ListRefresh => {
                self.last_list = Some(now);
                self.list_at = now + self.timings.list_refresh;
            }
        }
    }

    /// A transport error: try the next server after a doubling backoff
    /// (`backoff_start`, capped at `backoff_cap`), and log in again.
    pub fn on_error(&mut self, now: Instant) {
        self.in_flight = false;
        let next = match self.backoff {
            None => self.timings.backoff_start,
            Some(b) => (b * 2).min(self.timings.backoff_cap),
        };
        self.backoff = Some(next);
        self.server = (self.server + 1) % self.server_count;
        self.needs_login = true;
        self.login_at = now + next;
    }

    /// A final answer (bad password, refusal): stop until [`Self::resume`].
    pub fn halt(&mut self) {
        self.in_flight = false;
        self.halted = true;
    }

    #[must_use]
    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Start over with a login at `now` (new credentials, say).
    pub fn resume(&mut self, now: Instant) {
        self.halted = false;
        self.in_flight = false;
        self.backoff = None;
        self.needs_login = true;
        self.login_at = now;
    }

    /// Ask for a list now. False when the last list came under
    /// `list_min_interval` ago.
    pub fn request_list(&mut self, now: Instant) -> bool {
        if self.halted {
            return false;
        }
        if let Some(last) = self.last_list
            && now.saturating_duration_since(last) < self.timings.list_min_interval
        {
            return false;
        }
        self.list_at = now;
        true
    }

    /// Which directory server to use.
    #[must_use]
    pub fn server_index(&self) -> usize {
        self.server
    }

    /// The backoff in force after the latest error, if any.
    #[must_use]
    pub fn current_backoff(&self) -> Option<Duration> {
        self.backoff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn logged_in(t0: Instant, servers: usize) -> DirectorySchedule {
        let mut sc = DirectorySchedule::new(Timings::default(), servers, t0);
        assert_eq!(sc.due(t0), Some(DirectoryJob::Login));
        sc.on_success(DirectoryJob::Login, t0);
        sc
    }

    fn settle_first_list(sc: &mut DirectorySchedule, t: Instant) {
        assert_eq!(sc.due(t), Some(DirectoryJob::ListRefresh));
        sc.on_success(DirectoryJob::ListRefresh, t);
    }

    #[test]
    fn status_refresh_every_300_s_after_login() {
        let t0 = Instant::now();
        let mut sc = logged_in(t0, 2);
        settle_first_list(&mut sc, t0);
        assert_eq!(sc.due(t0 + s(299)), None);
        assert_eq!(sc.due(t0 + s(300)), Some(DirectoryJob::StatusRefresh));
        sc.on_success(DirectoryJob::StatusRefresh, t0 + s(300));
        assert_eq!(sc.due(t0 + s(599)), None);
        assert_eq!(sc.due(t0 + s(600)), Some(DirectoryJob::StatusRefresh));
    }

    #[test]
    fn list_refresh_every_600_s() {
        let t0 = Instant::now();
        let mut sc = logged_in(t0, 2);
        settle_first_list(&mut sc, t0);
        assert_eq!(sc.due(t0 + s(300)), Some(DirectoryJob::StatusRefresh));
        sc.on_success(DirectoryJob::StatusRefresh, t0 + s(300));
        assert_eq!(sc.due(t0 + s(599)), None);
        // Both are due at 600 s; the status refresh goes first, then the list.
        assert_eq!(sc.due(t0 + s(600)), Some(DirectoryJob::StatusRefresh));
        sc.on_success(DirectoryJob::StatusRefresh, t0 + s(600));
        assert_eq!(sc.due(t0 + s(600)), Some(DirectoryJob::ListRefresh));
    }

    #[test]
    fn on_demand_list_is_refused_inside_30_s() {
        let t0 = Instant::now();
        let mut sc = logged_in(t0, 2);
        settle_first_list(&mut sc, t0);
        assert!(!sc.request_list(t0 + s(29)));
        assert!(sc.request_list(t0 + s(30)));
        assert_eq!(sc.due(t0 + s(30)), Some(DirectoryJob::ListRefresh));
    }

    #[test]
    fn backoff_doubles_from_20_s_to_a_600_s_cap() {
        let t0 = Instant::now();
        let mut sc = DirectorySchedule::new(Timings::default(), 3, t0);
        let mut now = t0;
        for want in [20, 40, 80, 160, 320, 600, 600] {
            assert_eq!(sc.due(now), Some(DirectoryJob::Login));
            sc.on_error(now);
            assert_eq!(sc.current_backoff(), Some(s(want)));
            assert_eq!(sc.next_due(), now + s(want));
            assert_eq!(sc.due(now + s(want - 1)), None);
            now += s(want);
        }
    }

    #[test]
    fn each_error_moves_to_the_next_server() {
        let t0 = Instant::now();
        let mut sc = DirectorySchedule::new(Timings::default(), 4, t0);
        let mut seen = vec![sc.server_index()];
        for _ in 0..4 {
            sc.on_error(t0);
            seen.push(sc.server_index());
        }
        assert_eq!(seen, [0, 1, 2, 3, 0]);
    }

    #[test]
    fn success_resets_the_backoff() {
        let t0 = Instant::now();
        let mut sc = DirectorySchedule::new(Timings::default(), 2, t0);
        sc.on_error(t0);
        sc.on_error(t0);
        assert_eq!(sc.current_backoff(), Some(s(40)));
        sc.on_success(DirectoryJob::Login, t0 + s(40));
        assert_eq!(sc.current_backoff(), None);
        sc.on_error(t0 + s(50));
        assert_eq!(sc.current_backoff(), Some(s(20)));
    }

    #[test]
    fn a_final_refusal_halts_without_backoff_until_resumed() {
        let t0 = Instant::now();
        let mut sc = DirectorySchedule::new(Timings::default(), 2, t0);
        assert_eq!(sc.due(t0), Some(DirectoryJob::Login));
        sc.halt();
        assert!(sc.is_halted());
        assert_eq!(sc.current_backoff(), None);
        assert_eq!(sc.due(t0 + s(10_000)), None);
        sc.resume(t0 + s(5));
        assert_eq!(sc.due(t0 + s(5)), Some(DirectoryJob::Login));
    }

    #[test]
    fn a_job_in_flight_is_not_handed_out_twice() {
        let t0 = Instant::now();
        let mut sc = DirectorySchedule::new(Timings::default(), 1, t0);
        assert_eq!(sc.due(t0), Some(DirectoryJob::Login));
        assert_eq!(sc.due(t0), None);
    }

    #[test]
    fn wake_at_is_none_while_halted_or_in_flight() {
        let t0 = Instant::now();
        let mut sc = DirectorySchedule::new(Timings::default(), 1, t0);
        assert_eq!(sc.wake_at(), Some(t0));
        assert_eq!(sc.due(t0), Some(DirectoryJob::Login));
        assert_eq!(sc.wake_at(), None);
        sc.on_error(t0);
        assert_eq!(sc.wake_at(), Some(t0 + s(20)));
        sc.halt();
        assert_eq!(sc.wake_at(), None);
    }

    #[test]
    fn request_list_on_a_halted_schedule_is_refused_and_changes_nothing() {
        let t0 = Instant::now();
        let mut sc = logged_in(t0, 1);
        settle_first_list(&mut sc, t0);
        let before = sc.next_due();
        sc.halt();
        assert!(!sc.request_list(t0 + s(100)));
        assert_eq!(sc.next_due(), before);
    }

    #[test]
    fn error_then_relogin_then_the_list_is_due() {
        let t0 = Instant::now();
        let mut sc = DirectorySchedule::new(Timings::default(), 2, t0);
        assert_eq!(sc.due(t0), Some(DirectoryJob::Login));
        sc.on_error(t0);
        assert_eq!(sc.due(t0 + s(19)), None);
        assert_eq!(sc.due(t0 + s(20)), Some(DirectoryJob::Login));
        sc.on_success(DirectoryJob::Login, t0 + s(20));
        assert_eq!(sc.due(t0 + s(20)), Some(DirectoryJob::ListRefresh));
    }
}
