// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The receive-only M17 monitor keeps no transmit vocabulary in its source.
//!
//! `src/m17/monitor.rs` must never name anything that builds or sends voice:
//! no encoder call, no link setup frame, no stream-packet literal, no PTT and
//! no transmit channel. Its only raw socket send is the one inside
//! `send_control`, which re-parses every buffer as a control packet. The scan
//! covers the whole file, comments included, so a later edit cannot add a
//! transmit path without deleting this test. It lives in its own file so the
//! forbidden list below never matches itself.
//!
//! A second socket would be a second wire path that `send_control` never
//! sees, so the scan also pins where the one socket comes from: no clone of
//! it, and outside the test module no bind, only the single
//! `connect_udp_socket_with` call whose result goes straight into
//! `ControlOnlySocket`. Serialising a received stream packet would put voice
//! bytes in hand for a send, so the shipped code may serialise exactly one
//! thing: its closing DISC.

const MONITOR_SRC: &str = include_str!("../src/m17/monitor.rs");

const FORBIDDEN: &[&str] = &[
    "encode(",
    "Lsf",
    "BROADCAST",
    "StreamPacket {",
    "TxState",
    "tx_frames",
    "set_ptt",
    "ptt",
    "send_voice_packet",
    "CallAudio",
    "send_to(",
    "try_clone",
    "StreamPacket::to_bytes",
];

/// The line pair that opens the monitor's test module. Everything above it is
/// the code that ships.
const TEST_MODULE: &str = "#[cfg(test)]\nmod tests {";

/// The monitor's shipped code, without its test module (whose fake
/// reflectors bind their own loopback sockets).
fn production_src() -> &'static str {
    MONITOR_SRC
        .split_once(TEST_MODULE)
        .map_or(MONITOR_SRC, |(production, _)| production)
}

#[test]
fn the_scan_reads_the_monitor_source() {
    assert_eq!(
        MONITOR_SRC.matches("fn send_control(").count(),
        1,
        "the include must be the monitor, whose only wire path is send_control"
    );
    assert_eq!(
        MONITOR_SRC.matches(TEST_MODULE).count(),
        1,
        "the production/test split must find exactly one test module"
    );
}

#[test]
fn monitor_source_names_no_transmit_vocabulary() {
    let found: Vec<&str> = FORBIDDEN
        .iter()
        .copied()
        .filter(|token| MONITOR_SRC.contains(*token))
        .collect();
    assert_eq!(
        found,
        Vec::<&str>::new(),
        "src/m17/monitor.rs must not name transmit vocabulary"
    );
}

#[test]
fn monitor_has_exactly_one_raw_socket_send() {
    // No leading dot: a send on the bare `socket` binding in `run`, before it
    // moves into `ControlOnlySocket`, counts too.
    assert_eq!(
        MONITOR_SRC.matches("socket.send(").count(),
        1,
        "the only raw send in src/m17/monitor.rs is the one inside send_control"
    );
    // The field is named exactly twice in shipped code (send_control's send
    // and recv), so it cannot be aliased (`let raw = &sock.socket;`) and sent
    // on under another name.
    assert_eq!(
        production_src().matches(".socket").count(),
        2,
        "shipped code names the socket field only in send_control and recv"
    );
}

#[test]
fn monitor_opens_exactly_one_socket() {
    assert_eq!(
        MONITOR_SRC.matches("connect_udp_socket_with(").count(),
        1,
        "src/m17/monitor.rs must open its socket in exactly one call"
    );
    assert_eq!(
        MONITOR_SRC.matches("connect_udp_socket(").count(),
        0,
        "the monitor goes through the injectable connect_udp_socket_with only"
    );
    assert_eq!(
        production_src().matches("UdpSocket::bind").count(),
        0,
        "shipped monitor code must not bind a socket of its own"
    );
}

#[test]
fn shipped_monitor_serialises_only_its_disc() {
    // Counted, not matched on the DISC's text: rustfmt may split that struct
    // literal across lines, but `.to_bytes()` itself stays whole.
    assert_eq!(
        production_src().matches(".to_bytes()").count(),
        1,
        "the only serialisation in shipped monitor code is the closing DISC"
    );
}
