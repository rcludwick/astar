// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The `EchoLink` password handed to `Station::echolink_login` never reaches
//! the station's `Debug`, its `EchoLink` state, its last error, any error it
//! returns, or a log line.
//!
//! The presence and link threads log on their own threads, and
//! `tracing::subscriber::with_default` only covers the calling thread, so
//! this file installs one global capturing subscriber at TRACE and holds a
//! single test. It is a test binary of its own so no other test shares that
//! subscriber.
#![cfg(feature = "echolink")]

use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use astar_console::echolink::{EchoLinkNetwork, LoginState, PeerAddr, StationEntry, Timings};
use astar_echolink::fake::{FakeDirectory, FakeLogin, FakeStation};
use astar_station::{Station, StationConfig};

const PASSWORD: &str = "s3cret-pw";
const NODE: u32 = 9999;

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

fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn station_for(fake: &FakeStation) -> Station {
    let addr = fake.addr();
    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    station.set_echolink_network(EchoLinkNetwork {
        audio_bind: "127.0.0.1:0".parse().unwrap(),
        control_bind: "127.0.0.1:0".parse().unwrap(),
        peer_addr: Arc::new(move |e: &StationEntry| {
            if e.node == NODE {
                addr
            } else {
                PeerAddr::fixed(e.ip)
            }
        }),
        match_by_ip: false,
        timings: Timings {
            keepalive: Duration::from_millis(100),
            connection_timeout: Duration::from_millis(2_000),
            give_up_after: 20,
            talk_hold: Duration::from_millis(100),
            ..Timings::default()
        },
    });
    station
}

#[test]
fn the_password_never_reaches_a_station_debug_state_error_or_log() {
    let captured = capture();
    let mut texts = Vec::new();
    let fake = FakeStation::start("*ECHOTEST*");

    // A good login, a call, a chat line, a hang-up and a logout.
    let good = FakeDirectory::start(FakeLogin::Ok, vec![fake.entry(NODE)]);
    let station = station_for(&fake);
    station
        .echolink_login("KX7ABC", PASSWORD.to_owned(), &[good.addr().to_string()])
        .expect("login starts");
    assert!(wait_for(|| station
        .echolink_state()
        .is_some_and(|s| s.login.state == LoginState::LoggedIn)
        && !station.echolink_stations().is_empty()));
    station.echolink_connect("9999").expect("connect");
    station.echolink_send_chat("hello").expect("chat");
    let debug = format!("{station:?}");
    assert!(debug.contains("echolink"), "{debug}");
    assert!(
        debug.contains("KX7ABC"),
        "the Debug shows the state: {debug}"
    );
    texts.push(debug);
    texts.push(format!("{:?}", station.echolink_state()));
    texts.push(format!("{:?}", station.echolink_stations()));
    texts.push(format!("{:?}", station.snapshot()));
    station.echolink_disconnect();
    station.echolink_logout();
    assert!(
        good.lines()
            .iter()
            .any(|l| l.windows(PASSWORD.len()).any(|w| w == PASSWORD.as_bytes())),
        "the password did go to the directory"
    );

    // A bad password: the state, the last error, and the refused call.
    let bad = FakeDirectory::start(FakeLogin::BadPassword, Vec::new());
    station
        .echolink_login("KX7ABC", PASSWORD.to_owned(), &[bad.addr().to_string()])
        .expect("login starts");
    assert!(wait_for(|| station
        .echolink_state()
        .is_some_and(|s| s.login.state == LoginState::Failed)));
    texts.push(format!("{:?}", station.echolink_state()));
    texts.push(station.echolink_last_error().expect("an error"));
    let refused = station.echolink_connect("9999").expect_err("not logged in");
    texts.push(refused.to_string());
    texts.push(format!("{refused:?}"));

    // Logins refused before they start: each error, both ways it prints.
    for refused in [
        station.echolink_login("", PASSWORD.to_owned(), &[bad.addr().to_string()]),
        station.echolink_login("KX7ABC", PASSWORD.to_owned(), &[]),
    ] {
        let e = refused.expect_err("refused");
        texts.push(e.to_string());
        texts.push(format!("{e:?}"));
    }

    texts.push(format!("{station:?}"));
    texts.push(format!("{:?}", station.snapshot()));
    station.echolink_logout();
    drop(station);

    let log = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
    assert!(!log.is_empty(), "nothing was logged");
    assert!(
        !log.contains(PASSWORD),
        "the password is in the log:\n{log}"
    );
    for text in &texts {
        assert!(!text.contains(PASSWORD), "the password is in {text}");
    }
}
