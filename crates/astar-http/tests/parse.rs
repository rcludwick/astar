// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
use astar_http::parse::{HeadError, read_head};
use std::io::BufReader;

fn parse(raw: &[u8]) -> Result<Option<astar_http::parse::RequestHead>, HeadError> {
    read_head(&mut BufReader::new(raw), 8 * 1024)
}

#[test]
fn a_plain_get_parses() {
    let h = parse(b"GET /a?b=1 HTTP/1.1\r\nHost: x\r\nX-Y:  z \r\n\r\n")
        .unwrap()
        .unwrap();
    assert_eq!(
        (h.method.as_str(), h.path(), h.query(), h.version.as_str()),
        ("GET", "/a", "b=1", "HTTP/1.1")
    );
    assert_eq!(h.header("host"), Some("x"));
    assert_eq!(h.header("x-y"), Some("z"));
}

#[test]
fn an_oversized_head_is_431() {
    let raw = format!(
        "GET / HTTP/1.1\r\nHost: x\r\nX: {}\r\n\r\n",
        "a".repeat(9000)
    );
    assert_eq!(parse(raw.as_bytes()).unwrap_err().status(), Some(431));
}

#[test]
fn a_cut_short_head_and_invalid_utf8_are_400() {
    assert_eq!(
        parse(b"GET / HTTP/1.1\r\nHost: x\r\n")
            .unwrap_err()
            .status(),
        Some(400)
    );
    assert_eq!(
        parse(b"GET /\xff HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap_err()
            .status(),
        Some(400)
    );
}

#[test]
fn malformed_lines_are_400() {
    for raw in [
        &b"GET / HTTP/2.0\r\nHost: x\r\n\r\n"[..],
        b"GET\r\n\r\n",
        b"GET http://x/ HTTP/1.1\r\nHost: x\r\n\r\n", // not origin-form: no proxying
        b"GET / HTTP/1.1\r\nHost x\r\n\r\n",          // no colon
        b"GET / HTTP/1.1\r\nHost : x\r\n\r\n",        // space before the colon
        b"GET / HTTP/1.1\r\nHost: x\r\n folded\r\n\r\n", // obs-fold
        b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: x\r\nHost: y\r\n\r\n",
        b"GET /a\rb HTTP/1.1\r\nHost: x\r\n\r\n", // bare CR in the target
        b"GET / HTTP/1.1\r\nHost: x\r\nX: a\rContent-Length: 5\r\n\r\n", // bare CR in a value
        b"GET / HTTP/1.1\r\nHost: x\r\nX: a\0b\r\n\r\n", // NUL in a value
        b"GET / HTTP/1.1\r\nHost: x\r\nX: a\x7fb\r\n\r\n", // DEL in a value
        b"GET / HTTP/1.1\r\nHost: x\r\nX: a\r\r\n\r\n", // doubled CR before the LF
        b"GET /\xc3\xa9 HTTP/1.1\r\nHost: x\r\n\r\n", // non-ASCII target
        b"GET / HTTP/1.1\r\nHost: x\r\nX\x0bY: z\r\n\r\n", // VT in a name
        b"GET / HTTP/1.1\r\nHost: x\r\nTransfer\xe2\x80\x91Encoding: z\r\n\r\n", // non-ASCII name
        b"GET / HTTP/1.1\r\nHost: x\r\nX@Y: z\r\n\r\n",
    ] {
        assert_eq!(
            parse(raw).unwrap_err().status(),
            Some(400),
            "{}",
            String::from_utf8_lossy(raw)
        );
    }
}

#[test]
fn a_connection_closed_before_a_request_is_none() {
    assert!(parse(b"").unwrap().is_none());
}

#[test]
fn only_one_request_is_read() {
    let raw = b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\nHost: x\r\n\r\n";
    let mut r = BufReader::new(&raw[..]);
    let h = read_head(&mut r, 8192).unwrap().unwrap();
    assert_eq!(h.path(), "/a");
}

#[test]
fn a_tab_in_a_value_is_fine() {
    let h = parse(b"GET / HTTP/1.1\r\nHost: x\r\nX: a\tb\r\n\r\n")
        .unwrap()
        .unwrap();
    assert_eq!(h.header("x"), Some("a\tb"));
}
