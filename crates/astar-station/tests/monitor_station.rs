// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The speaker monitor through `Station`: the device, gain and mute are held
//! by the session across calls, and the monitor plays while an output bus is
//! open. Hermetic: a `NullBackend` with two synthetic outputs opens no
//! device, and the M17 link goes to a fake reflector on `127.0.0.1`.
#![cfg(feature = "m17")]

use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use astar_station::{CallStatus, MONITOR_SAME_DEVICE, MonitorState, Station, StationConfig};

/// A station whose backend offers two outputs: `out:null` (the default, so
/// the call's bus opens there) and `out:mon` for the monitor.
fn two_output_station() -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| {
            Box::new(astar_audio::NullBackend::new().with_outputs(&["out:null", "out:mon"]))
        }),
    )
}

/// CONN (11 bytes) → ACKN, then silence. One reflector acknowledges one
/// CONN, so each link spawns its own.
fn spawn_reflector() -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    thread::spawn(move || {
        let mut buf = [0u8; 4_096];
        let mut acked = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let Ok((n, src)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if !acked && n == 11 && &buf[0..4] == b"CONN" {
                acked = true;
                let _ = sock.send_to(b"ACKN", src);
            }
        }
    });
    addr
}

fn link(station: &Station) {
    let addr = spawn_reflector();
    station
        .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("m17 connect to the loopback reflector");
    let deadline = Instant::now() + Duration::from_secs(2);
    while station.snapshot().status != CallStatus::Answered && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(station.snapshot().status, CallStatus::Answered, "must link");
}

#[test]
fn monitor_off_by_default() {
    let station = two_output_station();
    assert_eq!(
        station.monitor_state(),
        MonitorState::Off,
        "idle, no device"
    );
    link(&station);
    assert_eq!(
        station.monitor_state(),
        MonitorState::Off,
        "a call with no monitor device chosen leaves the monitor off"
    );
    station.m17_disconnect();
}

#[test]
fn monitor_setting_survives_reconnect() {
    let station = two_output_station();
    station
        .set_monitor_device(Some("out:mon".to_string()))
        .expect("choosing a monitor before any call");
    station.set_monitor_gain(0.5);
    station.set_monitor_muted(true);
    assert_eq!(
        station.monitor_state(),
        MonitorState::Off,
        "a chosen monitor is off while no bus is open"
    );

    link(&station);
    assert_eq!(station.monitor_state(), MonitorState::Playing);

    station.m17_disconnect();
    assert_eq!(
        station.monitor_state(),
        MonitorState::Off,
        "hanging up closes the last bus, and the monitor with it"
    );

    link(&station);
    assert_eq!(
        station.monitor_state(),
        MonitorState::Playing,
        "the choice outlives the call"
    );
    station.m17_disconnect();
}

#[test]
fn choosing_and_clearing_the_monitor_mid_call() {
    let station = two_output_station();
    link(&station);
    station
        .set_monitor_device(Some("mon".to_string()))
        .expect("a substring names the device, as outputs are named");
    assert_eq!(station.monitor_state(), MonitorState::Playing);
    station.set_monitor_device(None).expect("turning it off");
    assert_eq!(station.monitor_state(), MonitorState::Off);
    station.m17_disconnect();
}

#[test]
fn a_monitor_on_the_output_device_is_unavailable() {
    let station = two_output_station();
    station
        .set_monitor_device(Some("out:null".to_string()))
        .expect("the choice is accepted");
    link(&station);
    assert_eq!(
        station.monitor_state(),
        MonitorState::Unavailable(MONITOR_SAME_DEVICE.to_string())
    );
    station.m17_disconnect();
}

#[test]
fn an_empty_device_name_turns_the_monitor_off() {
    let station = two_output_station();
    station
        .set_monitor_device(Some("out:mon".to_string()))
        .expect("choose");
    station
        .set_monitor_device(Some(String::new()))
        .expect("clear");
    link(&station);
    assert_eq!(station.monitor_state(), MonitorState::Off);
    station.m17_disconnect();
}
