// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#![cfg(feature = "testing")]

use std::io::{Read, Write};

#[test]
fn a_stub_answers_records_and_stops_on_drop() {
    let stub = astar_http::testing::serve(|req| {
        astar_http::Response::json(200, format!("{{\"path\":\"{}\"}}", req.path()))
    });
    assert!(stub.url().starts_with("http://127.0.0.1:"));
    let mut s = std::net::TcpStream::connect(stub.addr()).unwrap();
    s.write_all(b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\nhi")
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.ends_with("{\"path\":\"/a\"}"), "{out}");
    let seen = stub.requests();
    assert_eq!(
        (seen[0].method.as_str(), seen[0].body_str()),
        ("POST", "hi")
    );
    let addr = stub.addr();
    drop(stub);
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(std::net::TcpStream::connect(addr).is_err());
}
