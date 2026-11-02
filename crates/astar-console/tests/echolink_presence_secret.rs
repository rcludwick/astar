// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The `EchoLink` password never reaches a log line, a snapshot or an error.
//!
//! The presence thread logs on its own thread, and
//! `tracing::subscriber::with_default` only covers the calling thread, so this
//! file installs one global capturing subscriber at TRACE and holds a single
//! test. It is a test binary of its own so no other test shares that
//! subscriber.
#![cfg(feature = "echolink")]

use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use astar_console::echolink::{EchoLinkLogin, EchoLinkPresence, LoginState};
use astar_echolink::Timings;
use astar_echolink::fake::{FakeDirectory, FakeLogin};
use zeroize::Zeroizing;

const PASSWORD: &str = "s3cret-pw";

static CAPTURE: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Install the global subscriber once and return what it has captured.
fn capture() -> &'static Arc<Mutex<Vec<u8>>> {
    CAPTURE.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buf);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || Capture(Arc::clone(&writer)))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        buf
    })
}

fn timings() -> Timings {
    Timings {
        status_refresh: Duration::from_millis(200),
        list_refresh: Duration::from_millis(400),
        list_min_interval: Duration::from_millis(50),
        backoff_start: Duration::from_millis(50),
        backoff_cap: Duration::from_millis(200),
        ..Timings::default()
    }
}

fn start(dir: &FakeDirectory) -> EchoLinkPresence {
    EchoLinkPresence::start(
        EchoLinkLogin {
            callsign: "KX7ABC".to_owned(),
            servers: vec![dir.addr().to_string()],
            location: "Denver".to_owned(),
        },
        Zeroizing::new(PASSWORD.to_owned()),
        timings(),
    )
    .expect("presence starts")
}

fn wait_for_state(presence: &EchoLinkPresence, want: LoginState) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while presence.snapshot().state != want {
        assert!(Instant::now() < deadline, "{:?}", presence.snapshot());
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_password_appears_in_no_log_snapshot_or_error() {
    let captured = capture();
    let mut texts = Vec::new();

    // A good login, a status refresh and a list, then a logout.
    let good = FakeDirectory::start(FakeLogin::Ok, Vec::new());
    let presence = start(&good);
    wait_for_state(&presence, LoginState::LoggedIn);
    thread::sleep(Duration::from_millis(300));
    texts.push(format!("{:?}", presence.snapshot()));
    presence.logout();
    assert!(
        good.lines()
            .iter()
            .any(|l| l.windows(PASSWORD.len()).any(|w| w == PASSWORD.as_bytes())),
        "the password did go to the directory"
    );

    // A bad password: the error sentence and the snapshot.
    let bad = FakeDirectory::start(FakeLogin::BadPassword, Vec::new());
    let presence = start(&bad);
    wait_for_state(&presence, LoginState::Failed);
    let snapshot = presence.snapshot();
    texts.push(format!("{snapshot:?}"));
    texts.push(snapshot.error.clone().expect("an error"));
    presence.logout();

    // A start that is refused: the console error, both ways it prints.
    let refused = EchoLinkPresence::start(
        EchoLinkLogin {
            callsign: "KX7ABC".to_owned(),
            servers: Vec::new(),
            location: String::new(),
        },
        Zeroizing::new(PASSWORD.to_owned()),
        timings(),
    )
    .err()
    .expect("no servers is refused");
    texts.push(refused.to_string());
    texts.push(format!("{refused:?}"));

    let log = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
    assert!(!log.is_empty(), "the presence thread logged nothing");
    assert!(
        !log.contains(PASSWORD),
        "the password is in the log:\n{log}"
    );
    for text in &texts {
        assert!(!text.contains(PASSWORD), "the password is in {text}");
    }
}
