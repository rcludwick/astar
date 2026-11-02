// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Staying listed in the `EchoLink` directory.
//!
//! A station that wants to be reachable, or just wants the station list, logs
//! in to a directory server and then keeps telling it "still here". Each of
//! those is one short TCP exchange: connect, send a line, read the reply, and
//! the server closes. [`EchoLinkPresence`] runs those exchanges on a thread of
//! its own, `astar-echolink-dir`, on the timetable [`DirectorySchedule`] keeps:
//! a login, a status refresh every 300 s, a station list every 600 s (or on
//! request), and a doubling back-off across the servers when one cannot be
//! reached.
//!
//! The thread sleeps until the schedule's next job comes due. While a job is
//! in flight, or after the directory has said no for good, the schedule has no
//! next time, and the thread then waits for a command instead of spinning.
//!
//! Two kinds of failure are told apart. A wrong password, or any other reply
//! that is a refusal, is final: retrying would get the same answer, so the
//! thread stops talking to the directory until the next login. A transport
//! failure (no connection, no answer in time, the server closing without a
//! word, a list that does not parse) backs off and tries the next server.
//!
//! The password is moved into the thread as a `Zeroizing<String>` and lives
//! there until logout, because every status refresh sends it again. Each
//! line that carries it is built into a zeroed-on-drop buffer and dropped
//! right after the write. It is never in a snapshot, an error or a log line.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use astar_echolink::Timings;
use astar_echolink::directory::{
    LIST_REQUEST, ListParser, LoginReply, StationEntry, Status, login_line, logoff_line,
    parse_login_reply,
};
use astar_echolink::schedule::{DirectoryJob, DirectorySchedule};
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use super::{EchoLinkLogin, LoginState, PresenceSnapshot, net};
use crate::ConsoleError;

const BAD_PASSWORD: &str = "EchoLink said the password is wrong";
const UNREACHABLE: &str = "Couldn't reach any EchoLink server";

/// The sentence for a login the directory refused, quoting its reply.
fn refused(reply: &str) -> String {
    format!(
        "EchoLink refused the login (\"{reply}\"). If you registered recently, \
         your callsign may not be validated yet."
    )
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The local wall-clock time, which every status line carries.
fn local_hhmm() -> (u8, u8) {
    let now = jiff::Zoned::now();
    (
        u8::try_from(now.hour()).unwrap_or(0),
        u8::try_from(now.minute()).unwrap_or(0),
    )
}

/// What the presence thread has to say, shared with the handle.
struct Progress {
    state: LoginState,
    error: Option<String>,
    retrying: bool,
    stations_at: Option<Instant>,
}

struct Shared {
    callsign: String,
    progress: Mutex<Progress>,
    stations: Mutex<Arc<Vec<StationEntry>>>,
    /// Held only between exchanges, never across one, so `refresh_list` never
    /// waits on the network.
    schedule: Mutex<DirectorySchedule>,
}

enum Command {
    /// Look at the schedule again (a list was asked for).
    Wake,
    SetLocation(String),
    Logout,
}

/// A login to the `EchoLink` directory, kept alive by a thread of its own.
///
/// Dropping it logs off the same way [`EchoLinkPresence::logout`] does.
pub struct EchoLinkPresence {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl EchoLinkPresence {
    /// Start logging in. Returns at once; the login happens on the thread,
    /// and [`Self::snapshot`] shows how it went.
    ///
    /// Refused when there is no callsign or no directory server.
    pub fn start(
        login: EchoLinkLogin,
        password: Zeroizing<String>,
        timings: Timings,
    ) -> Result<EchoLinkPresence, ConsoleError> {
        Self::start_replacing(login, password, timings, &mut None)
    }

    /// [`Self::start`], taking over from the login in `previous`.
    ///
    /// The new thread logs `previous` off (its logoff line goes out) before
    /// it sends its own first login, so a re-login with the same callsign
    /// never ends with the directory hearing the old logoff last. The
    /// caller does not wait for that logoff: it happens on the new thread.
    ///
    /// `previous` is taken only once the new login has passed its checks; a
    /// refused login leaves it in place, still running.
    pub(crate) fn start_replacing(
        login: EchoLinkLogin,
        password: Zeroizing<String>,
        timings: Timings,
        previous: &mut Option<EchoLinkPresence>,
    ) -> Result<EchoLinkPresence, ConsoleError> {
        let callsign = login.callsign.trim().to_owned();
        if callsign.is_empty() {
            return Err(ConsoleError::EchoLink(
                "EchoLink needs a callsign to log in".to_owned(),
            ));
        }
        let servers: Vec<String> = login
            .servers
            .iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if servers.is_empty() {
            return Err(ConsoleError::EchoLink(
                "EchoLink needs at least one directory server".to_owned(),
            ));
        }
        let shared = Arc::new(Shared {
            callsign: callsign.clone(),
            progress: Mutex::new(Progress {
                state: LoginState::LoggingIn,
                error: None,
                retrying: false,
                stations_at: None,
            }),
            stations: Mutex::new(Arc::new(Vec::new())),
            schedule: Mutex::new(DirectorySchedule::new(
                timings.clone(),
                servers.len(),
                Instant::now(),
            )),
        });
        let (commands, inbox) = mpsc::channel();
        let previous = previous.take();
        let worker = Worker {
            shared: Arc::clone(&shared),
            inbox,
            callsign,
            password,
            location: login.location,
            servers,
            timings,
            listed: false,
            failures: 0,
        };
        let thread = thread::Builder::new()
            .name("astar-echolink-dir".into())
            .spawn(move || {
                if let Some(previous) = previous {
                    previous.logout();
                }
                worker.run();
            })
            .map_err(|e| {
                ConsoleError::EchoLink(format!("couldn't start the directory thread: {e}"))
            })?;
        Ok(EchoLinkPresence {
            shared,
            commands,
            thread: Some(thread),
        })
    }

    /// Where the login stands right now.
    #[must_use]
    pub fn snapshot(&self) -> PresenceSnapshot {
        let progress = lock(&self.shared.progress);
        PresenceSnapshot {
            state: progress.state,
            callsign: self.shared.callsign.clone(),
            error: progress.error.clone(),
            retrying: progress.retrying,
            stations_age: progress.stations_at.map(|t| t.elapsed()),
        }
    }

    /// The latest station list (empty until the first one arrives).
    #[must_use]
    pub fn stations(&self) -> Arc<Vec<StationEntry>> {
        Arc::clone(&lock(&self.shared.stations))
    }

    /// Look a station up by node number, or else by callsign (any case).
    #[must_use]
    pub fn find(&self, node_or_call: &str) -> Option<StationEntry> {
        let wanted = node_or_call.trim();
        let stations = self.stations();
        if let Ok(node) = wanted.parse::<u32>() {
            return stations.iter().find(|s| s.node == node).cloned();
        }
        stations
            .iter()
            .find(|s| s.call.eq_ignore_ascii_case(wanted))
            .cloned()
    }

    /// Ask for a fresh station list now. False when the last one came less
    /// than `list_min_interval` ago, or the directory has refused the login.
    #[must_use]
    pub fn refresh_list(&self) -> bool {
        let asked = lock(&self.shared.schedule).request_list(Instant::now());
        if asked {
            let _ = self.commands.send(Command::Wake);
        }
        asked
    }

    /// Change the location text; it goes out with the next status refresh.
    pub fn set_location(&self, text: &str) {
        let _ = self.commands.send(Command::SetLocation(text.to_owned()));
    }

    /// Log off (one logoff line, when the station is listed) and stop the
    /// thread. Waits for an exchange already under way to finish first.
    ///
    /// # Blocking
    /// The worst case is one exchange in flight plus the logoff, each bounded
    /// by `directory_connect_timeout` + `directory_read_timeout`: about 80 s
    /// with the default timings. A host-name lookup has no deadline of its
    /// own in a production build (the system resolver decides), so a stalled
    /// DNS server can stretch that further. A test build refuses names, so it
    /// never looks one up.
    pub fn logout(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.commands.send(Command::Logout);
            let _ = thread.join();
        }
    }
}

/// Dropping logs off and joins, exactly as [`EchoLinkPresence::logout`]
/// does, and can block as long (see its "Blocking" section).
impl Drop for EchoLinkPresence {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How a job ended.
enum Outcome {
    Done,
    /// The directory said no for good: this sentence, no retry.
    Final(String),
    /// A transport failure, described for the log (never the request).
    Transport(String),
}

/// The presence thread's own state. It owns the password.
struct Worker {
    shared: Arc<Shared>,
    inbox: Receiver<Command>,
    callsign: String,
    password: Zeroizing<String>,
    location: String,
    servers: Vec<String>,
    timings: Timings,
    /// The directory has the station listed, so a logoff is owed.
    listed: bool,
    /// Transport failures in a row.
    failures: usize,
}

impl Worker {
    fn run(mut self) {
        debug!(callsign = %self.callsign, "echolink: directory thread started");
        loop {
            let wake = lock(&self.shared.schedule).wake_at();
            let received = match wake {
                None => self
                    .inbox
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
                Some(at) => self
                    .inbox
                    .recv_timeout(at.saturating_duration_since(Instant::now())),
            };
            match received {
                Ok(Command::Logout) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Command::SetLocation(text)) => self.location = text,
                Ok(Command::Wake) | Err(RecvTimeoutError::Timeout) => {}
            }
            let (job, server) = {
                let mut schedule = lock(&self.shared.schedule);
                (schedule.due(Instant::now()), schedule.server_index())
            };
            if let Some(job) = job {
                self.run_job(job, server);
            }
        }
        self.log_off();
        self.set_progress(LoginState::LoggedOut, None, false);
        debug!("echolink: directory thread stopped");
    }

    fn run_job(&mut self, job: DirectoryJob, server: usize) {
        debug!(?job, server = %self.servers[server], "echolink: directory job");
        let outcome = match job {
            DirectoryJob::Login | DirectoryJob::StatusRefresh => self.send_status(server),
            DirectoryJob::ListRefresh => self.fetch_list(server),
        };
        let now = Instant::now();
        match outcome {
            Outcome::Done => {
                self.failures = 0;
                lock(&self.shared.schedule).on_success(job, now);
                if job == DirectoryJob::Login {
                    self.listed = true;
                    info!(callsign = %self.callsign, "echolink: logged in to the directory");
                }
                if job != DirectoryJob::ListRefresh {
                    self.set_progress(LoginState::LoggedIn, None, false);
                }
            }
            Outcome::Final(sentence) => {
                lock(&self.shared.schedule).halt();
                self.listed = false;
                warn!("echolink: {sentence}");
                self.set_progress(LoginState::Failed, Some(sentence), false);
            }
            Outcome::Transport(why) => {
                self.failures += 1;
                let backoff = {
                    let mut schedule = lock(&self.shared.schedule);
                    schedule.on_error(now);
                    schedule.current_backoff()
                };
                warn!(
                    ?job,
                    server = %self.servers[server],
                    ?backoff,
                    "echolink: directory exchange failed: {why}"
                );
                // Once the directory has accepted the login, a failed
                // exchange is a hiccup: the station is still listed, so it
                // stays logged in and the back-off retries quietly. Only a
                // final refusal or a logout ends that. Before the first
                // success, the state says how the login is going. Only once
                // every server has failed in a row is "any" true.
                if !self.listed {
                    if self.failures >= self.servers.len() {
                        self.set_progress(LoginState::Failed, Some(UNREACHABLE.to_owned()), true);
                    } else {
                        self.set_progress(LoginState::LoggingIn, None, true);
                    }
                }
            }
        }
    }

    /// One exchange with `request`, on the server at `server`.
    fn exchange(&self, server: usize, request: &[u8]) -> Result<Vec<u8>, String> {
        let addr = net::resolve_server(&self.servers[server]).map_err(|e| e.to_string())?;
        net::directory_exchange(
            addr,
            request,
            self.timings.directory_connect_timeout,
            self.timings.directory_read_timeout,
        )
        .map_err(|e| e.to_string())
    }

    /// A login or status refresh: the same line, sent BUSY because astar
    /// does not take incoming calls.
    fn send_status(&self, server: usize) -> Outcome {
        let reply = {
            let line = login_line(
                &self.callsign,
                &self.password,
                Status::Busy,
                local_hhmm(),
                &self.location,
            );
            self.exchange(server, &line)
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(why) => return Outcome::Transport(why),
        };
        // With `eof` true the parser always reaches a verdict; `None` is
        // only possible for a reply still arriving, so it cannot occur here.
        // A refusal with no readable text (an empty reply, line ends only,
        // or bytes that are all unprintable) is not the directory saying no:
        // nobody can tell what was said, so it backs off like any other
        // transport failure instead of halting for good.
        match parse_login_reply(&reply, true) {
            Some(LoginReply::Ok) => Outcome::Done,
            Some(LoginReply::BadPassword) => Outcome::Final(BAD_PASSWORD.to_owned()),
            Some(LoginReply::Refused(text)) if !text.is_empty() => Outcome::Final(refused(&text)),
            Some(LoginReply::Refused(_)) | None => {
                Outcome::Transport("the server sent no readable reply".to_owned())
            }
        }
    }

    fn fetch_list(&self, server: usize) -> Outcome {
        let reply = match self.exchange(server, LIST_REQUEST) {
            Ok(reply) => reply,
            Err(why) => return Outcome::Transport(why),
        };
        let mut parser = ListParser::new();
        let list = match parser.push(&reply).and_then(|()| parser.finish()) {
            Ok(list) => list,
            Err(e) => return Outcome::Transport(format!("the station list was refused: {e}")),
        };
        debug!(
            stations = list.stations.len(),
            "echolink: station list received"
        );
        *lock(&self.shared.stations) = Arc::new(list.stations);
        lock(&self.shared.progress).stations_at = Some(Instant::now());
        Outcome::Done
    }

    /// Send the logoff line once, if the directory has the station listed.
    fn log_off(&mut self) {
        if !self.listed {
            return;
        }
        self.listed = false;
        let server = lock(&self.shared.schedule).server_index();
        let result = {
            let line = logoff_line(&self.callsign, &self.password, &self.location);
            self.exchange(server, &line)
        };
        match result {
            Ok(_) => info!(callsign = %self.callsign, "echolink: logged off the directory"),
            Err(why) => warn!("echolink: logoff failed: {why}"),
        }
    }

    /// Record where the login stands. `retrying` is `true` only on the
    /// back-off path before the first success; see
    /// [`PresenceSnapshot::retrying`].
    fn set_progress(&self, state: LoginState, error: Option<String>, retrying: bool) {
        let mut progress = lock(&self.shared.progress);
        progress.state = state;
        progress.error = error;
        progress.retrying = retrying;
    }
}
