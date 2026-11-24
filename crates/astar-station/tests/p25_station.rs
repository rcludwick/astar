// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Station`'s P25 facade, with the feature OFF and ON, and never on the air:
//! every address here is `127.0.0.1`.
//!
//! P25 has no dongle path: its vocoder is the software IMBE island, so a
//! connect here opens a real link to a loopback `P25Reflector` and a real
//! decoder, with a `NullBackend` for audio. Nothing opens a serial port, and
//! nothing probes a `ThumbDV`.
//!
//! The argument refusals run BEFORE the `#[cfg]` block, so they hold
//! identically with the feature off.

use astar_station::{Station, StationConfig, StationError};

fn test_station() -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    )
}

#[test]
fn an_empty_callsign_is_refused_before_anything_opens() {
    let st = test_station();
    let e = st.p25_connect("127.0.0.1:41000", "");
    assert!(matches!(e, Err(StationError::P25(_))), "got {e:?}");
    assert!(!st.snapshot().p25_active);
}

#[test]
fn an_eleven_character_callsign_is_refused_with_its_reason() {
    let st = test_station();
    let e = st.p25_connect("127.0.0.1:41000", "N0CALLN0CAL");
    match e {
        Err(StationError::P25(m)) => assert!(m.contains("longer than 10"), "got {m}"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn an_empty_host_is_refused() {
    let st = test_station();
    let e = st.p25_connect("", "N0CALL");
    match e {
        Err(StationError::P25(m)) => assert!(m.contains("must not be empty"), "got {m}"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn a_blank_host_is_refused_as_empty() {
    let st = test_station();
    for blank in ["   ", "\t", " \n "] {
        match st.p25_connect(blank, "N0CALL") {
            Err(StationError::P25(m)) => assert!(m.contains("must not be empty"), "got {m}"),
            other => panic!("{blank:?} gave {other:?}"),
        }
    }
}

#[test]
fn a_refused_argument_leaves_no_route_reserved() {
    let st = test_station();
    assert!(st.p25_connect("127.0.0.1:41000", "").is_err());
    // A later connect fails at its own validation, not with
    // `AlreadyConnected` from a lane the first attempt never gave back.
    match st.p25_connect("", "N0CALL") {
        Err(StationError::P25(_)) => {}
        other => panic!("the lane was not left alone: the retry gave {other:?}"),
    }
}

#[test]
fn the_error_displays_as_a_p25_error() {
    assert_eq!(
        StationError::P25("x".into()).to_string(),
        "p25 error: x".to_string()
    );
}

#[test]
fn p25_compiled_matches_the_feature_and_the_station() {
    assert_eq!(astar_station::p25_compiled(), cfg!(feature = "p25"));
    assert_eq!(test_station().p25_available(), cfg!(feature = "p25"));
    assert_eq!(
        test_station().snapshot().p25_available,
        cfg!(feature = "p25")
    );
}

#[cfg(not(feature = "p25"))]
#[test]
fn without_the_feature_connect_says_so() {
    let st = test_station();
    assert!(!st.p25_available());
    st.p25_disconnect(); // a no-op, must not panic
    let e = st.p25_connect("127.0.0.1:41000", "N0CALL");
    assert!(
        matches!(e, Err(StationError::P25(ref m)) if m == "p25 support not compiled"),
        "got {e:?}"
    );
}

#[cfg(feature = "p25")]
mod live {
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use astar_p25::{Reflector, ReflectorHandle};

    use super::{StationError, test_station};

    fn reflector() -> (ReflectorHandle, SocketAddr) {
        let r = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind loopback");
        let addr = r.local_addr();
        (r.run(), addr)
    }

    fn wait_for(st: &astar_station::Station, state: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let now = st.p25_state().map(|s| s.link_state);
            if now == Some(state) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "link never reached {state}; last {now:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn connect_links_to_a_loopback_reflector_and_set_ptt_is_refused() {
        let (refl, addr) = reflector();
        let st = test_station();
        st.p25_connect(&addr.to_string(), "N0CALL")
            .expect("connect to the loopback reflector");
        assert!(st.snapshot().p25_active);
        wait_for(&st, "linked");
        let s = st.p25_state().expect("a live link has state");
        assert_eq!(s.reflector, addr.to_string());
        assert!(!s.ptt);

        let e = st.set_ptt(true);
        assert!(matches!(e, Err(StationError::P25(_))), "got {e:?}");
        assert!(!st.snapshot().ptt, "a refused key must not key");

        st.p25_disconnect();
        assert!(!st.snapshot().p25_active);
        assert!(st.p25_state().is_none());
        refl.shutdown();
    }

    #[test]
    fn a_bare_host_gets_port_41000() {
        // Polls go to 127.0.0.1:41000 only; with nothing listening there the
        // link may stay "linking", which is fine.
        let st = test_station();
        st.p25_connect("127.0.0.1", "N0CALL")
            .expect("a bare loopback host is accepted");
        let s = st.p25_state().expect("a live link has state");
        assert_eq!(s.reflector, "127.0.0.1:41000");
        st.p25_disconnect();
    }

    /// Final review finding 5: surrounding whitespace is trimmed before
    /// the default port is added, so a pasted host links, not a resolve
    /// error.
    #[test]
    fn a_host_with_surrounding_whitespace_is_trimmed() {
        let st = test_station();
        st.p25_connect("  127.0.0.1 ", "N0CALL")
            .expect("a padded loopback host is accepted");
        let s = st.p25_state().expect("a live link has state");
        assert_eq!(s.reflector, "127.0.0.1:41000");
        st.p25_disconnect();

        let (refl, addr) = reflector();
        st.p25_connect(&format!(" {addr}\t"), "N0CALL")
            .expect("a padded host:port is accepted");
        wait_for(&st, "linked");
        st.p25_disconnect();
        refl.shutdown();
    }

    #[test]
    fn a_malformed_port_is_refused_as_a_p25_error() {
        let st = test_station();
        let e = st.p25_connect("127.0.0.1:", "N0CALL");
        assert!(matches!(e, Err(StationError::P25(_))), "got {e:?}");
        // And the lane went back: a good connect still works.
        st.p25_connect("127.0.0.1", "N0CALL")
            .expect("the lane was released after the refusal");
        st.p25_disconnect();
    }

    #[test]
    fn a_p25_link_and_an_nxdn_connect_are_mutually_exclusive() {
        let (refl, addr) = reflector();
        let st = test_station();
        st.p25_connect(&addr.to_string(), "N0CALL")
            .expect("connect");
        let e = st.nxdn_connect("127.0.0.1:41400", "N0CALL", 4242, 31_313);
        assert!(e.is_err(), "an NXDN connect must not succeed beside P25");
        let snap = st.snapshot();
        assert!(snap.p25_active);
        assert!(!snap.nxdn_active, "never both live");
        // A second P25 connect is refused too.
        assert!(matches!(
            st.p25_connect(&addr.to_string(), "N0CALL"),
            Err(StationError::AlreadyConnected)
        ));
        st.disconnect();
        assert!(!st.snapshot().p25_active, "disconnect ends the P25 link");
        refl.shutdown();
    }
}
