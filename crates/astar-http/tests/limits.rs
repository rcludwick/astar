// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
use astar_http::deadline::ReadDeadline;
use astar_http::limits::{
    Limits, PeerCounts, Pool, StreamClass, StreamGuard, StreamRefusal, peer_key,
};
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn ipv6_is_counted_per_64_and_mapped_v4_as_v4() {
    let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
    let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
    let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
    assert_eq!(peer_key(a), peer_key(b));
    assert_ne!(peer_key(a), peer_key(c));
    let mapped: IpAddr = "::ffff:192.0.2.7".parse().unwrap();
    assert_eq!(peer_key(mapped), "192.0.2.7".parse::<IpAddr>().unwrap());
}

#[test]
fn a_pool_caps_the_total_and_each_address() {
    let pool = Pool::new(3, 2);
    let x: IpAddr = "192.0.2.1".parse().unwrap();
    let y: IpAddr = "192.0.2.2".parse().unwrap();
    let a = pool.admit(x).unwrap();
    let _b = pool.admit(x).unwrap();
    assert!(pool.admit(x).is_none(), "a third from one address");
    assert_eq!(pool.open(), 2, "a refusal holds nothing");
    let _c = pool.admit(y).unwrap();
    assert!(pool.admit(y).is_none(), "the pool is full");
    drop(a);
    assert!(pool.admit(x).is_some(), "a freed slot is reusable");
}

#[test]
fn streams_are_capped_per_address_and_counted_overall() {
    let g = StreamGuard::new(2, 1);
    let x: IpAddr = "192.0.2.1".parse().unwrap();
    let y: IpAddr = "192.0.2.2".parse().unwrap();
    let ev = g.open(x, StreamClass::Counted).unwrap();
    let _audio = g.open(x, StreamClass::PerClientOnly).unwrap();
    assert_eq!(
        g.open(x, StreamClass::PerClientOnly).unwrap_err(),
        StreamRefusal::PerClient
    );
    assert_eq!(
        g.open(y, StreamClass::Counted).unwrap_err(),
        StreamRefusal::Full
    );
    assert_eq!(g.held(y), 0, "a refusal holds nothing");
    drop(ev);
    assert!(g.open(y, StreamClass::Counted).is_ok());
}

#[test]
fn refusals_keep_their_codes_and_texts() {
    assert_eq!(StreamRefusal::PerClient.code(), 429);
    assert_eq!(StreamRefusal::Full.code(), 503);
    assert_eq!(
        StreamRefusal::PerClient.text(),
        "Too many streams open from your address"
    );
    assert_eq!(StreamRefusal::Full.text(), "Full, try again shortly");
}

#[test]
fn the_two_presets_carry_the_agreed_numbers() {
    let p = Limits::public();
    assert_eq!(
        (
            p.max_connections,
            p.conns_per_client,
            p.deadline,
            p.max_head_bytes,
            p.max_body_bytes,
            p.write_stall,
            p.streams_per_client,
            p.max_counted_streams
        ),
        (
            1024,
            16,
            Duration::from_secs(10),
            8 * 1024,
            64 * 1024,
            Duration::from_secs(30),
            4,
            256
        )
    );
    let l = Limits::loopback();
    assert_eq!(
        (
            l.max_connections,
            l.conns_per_client,
            l.deadline,
            l.max_head_bytes,
            l.max_body_bytes,
            l.write_stall,
            l.streams_per_client,
            l.max_counted_streams
        ),
        (
            128,
            128,
            Duration::from_secs(10),
            8 * 1024,
            64 * 1024,
            Duration::from_secs(30),
            64,
            64
        )
    );
}

#[test]
fn peer_counts_hold_and_release() {
    let c = Arc::new(PeerCounts::default());
    let x: IpAddr = "192.0.2.1".parse().unwrap();
    let s = c.try_acquire(x, 1).unwrap();
    assert!(c.try_acquire(x, 1).is_none());
    assert_eq!(c.held(x), 1);
    drop(s);
    assert_eq!(c.held(x), 0);
}

#[test]
fn a_passed_deadline_refuses_reads() {
    let d = ReadDeadline::after(Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(d.remaining().unwrap_err().kind(), io::ErrorKind::TimedOut);
    d.clear();
    assert_eq!(d.remaining().unwrap(), None);
    // A stream's reads: each one bounded on its own, never "forever".
    d.per_read(Duration::from_millis(250));
    assert_eq!(d.remaining().unwrap(), Some(Duration::from_millis(250)));
}
