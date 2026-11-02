// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
mod support;
use astar_http::guards::{
    OriginRefusal, host_allowed, host_allowed_raw, is_json, is_loopback, is_loopback_peer,
    normalize_host, origin_rule, origin_rule_raw,
};
use astar_http::{Response, Server};
use support::{quick, send};

#[test]
fn normalize_host_keeps_todays_values() {
    assert_eq!(normalize_host("Node.Example.NET.:443"), "node.example.net");
    assert_eq!(normalize_host(" node.example.net "), "node.example.net");
    assert_eq!(normalize_host("node.example.net."), "node.example.net");
    assert_eq!(normalize_host("node.example.net:"), "node.example.net:");
    assert_eq!(normalize_host("node.example.net:8a"), "node.example.net:8a");
    assert_eq!(normalize_host(""), "");
    // An IPv6 literal keeps its brackets and its port, so it never matches
    // a name (today's behaviour, kept as it is).
    assert_eq!(normalize_host("[::1]:8730"), "[::1]:8730");
    assert_eq!(normalize_host("[::1]"), "[::1]");
}

#[test]
fn loopback_comes_from_the_peer_never_from_host() {
    let s = Server::bind("127.0.0.1:0", quick(), |req| {
        Response::text(200, is_loopback(req).to_string()).into()
    })
    .unwrap();
    let r = send(
        s.local_addr(),
        b"GET / HTTP/1.1\r\nHost: evil.example\r\n\r\n",
    );
    assert!(r.ends_with("true"), "{r}");
    for h in [
        "X-Forwarded-For: 198.51.100.7",
        "Forwarded: for=198.51.100.7",
    ] {
        let r = send(
            s.local_addr(),
            format!("GET / HTTP/1.1\r\nHost: localhost\r\n{h}\r\n\r\n").as_bytes(),
        );
        assert!(r.ends_with("false"), "{h}: {r}");
    }
    s.stop();
    let lan: std::net::SocketAddr = "192.0.2.5:5000".parse().unwrap();
    assert!(
        !is_loopback_peer(Some(lan), false),
        "Host: localhost from a LAN peer is not local"
    );
    assert!(is_loopback_peer(
        Some("[::ffff:127.0.0.1]:1".parse().unwrap()),
        false
    ));
    assert!(is_loopback_peer(Some("[::1]:1".parse().unwrap()), false));
    assert!(!is_loopback_peer(
        Some("127.0.0.1:1".parse().unwrap()),
        true
    ));
    assert!(!is_loopback_peer(None, false));
}

#[test]
fn the_origin_rule() {
    let json = Some("application/json; charset=utf-8");
    assert_eq!(
        origin_rule_raw(
            None,
            Some("cross-site"),
            Some("text/plain"),
            Some("127.0.0.1")
        ),
        Err(OriginRefusal::NotJson)
    );
    assert_eq!(
        origin_rule_raw(Some("https://evil.example"), None, json, Some("127.0.0.1")),
        Err(OriginRefusal::CrossOrigin)
    );
    assert_eq!(
        origin_rule_raw(Some("null"), None, json, Some("127.0.0.1")),
        Err(OriginRefusal::CrossOrigin)
    );
    assert_eq!(
        origin_rule_raw(
            Some("http://127.0.0.1:8730"),
            None,
            json,
            Some("127.0.0.1:8730")
        ),
        Ok(())
    );
    assert_eq!(
        origin_rule_raw(
            Some("http://NODE.example:8730"),
            None,
            json,
            Some("node.example:8730")
        ),
        Ok(()),
        "authority compared case-insensitively"
    );
    assert_eq!(
        origin_rule_raw(Some("http://127.0.0.1:8730"), None, json, None),
        Err(OriginRefusal::CrossOrigin),
        "no Host matches no Origin"
    );
    assert_eq!(
        origin_rule_raw(None, None, None, Some("127.0.0.1")),
        Ok(()),
        "curl keeps its contract"
    );
    assert_eq!(OriginRefusal::NotJson.status(), 415);
    assert_eq!(OriginRefusal::CrossOrigin.status(), 403);
    assert!(is_json(Some("Application/JSON ; charset=utf-8")));
    assert!(!is_json(Some("text/plain")));
    assert!(!is_json(None));
}

#[test]
fn a_cross_site_form_post_is_refused_over_the_socket() {
    let s = Server::bind("127.0.0.1:0", quick(), |req| match origin_rule(req) {
        Ok(()) => Response::text(200, "ok").into(),
        Err(e) => Response::text(e.status(), "refused").into(),
    })
    .unwrap();
    let r = send(
        s.local_addr(),
        b"POST /key HTTP/1.1\r\nHost: 127.0.0.1:8730\r\nOrigin: https://evil.example\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(r.starts_with("HTTP/1.1 415 "), "{r}");
    let r = send(
        s.local_addr(),
        b"POST /key HTTP/1.1\r\nHost: 127.0.0.1:8730\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(r.starts_with("HTTP/1.1 403 "), "{r}");
    let r = send(
        s.local_addr(),
        b"POST /key HTTP/1.1\r\nHost: 127.0.0.1:8730\r\nOrigin: http://127.0.0.1:8730\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(r.starts_with("HTTP/1.1 200 "), "{r}");
    s.stop();
}

#[test]
fn hosts_are_allowed_by_normalised_name() {
    let names = vec!["node.example.net".to_string()];
    assert!(host_allowed_raw(&names, Some("Node.Example.NET.:443")));
    assert!(!host_allowed_raw(&names, Some("evil.example")));
    assert!(!host_allowed_raw(&names, None));
    let s = Server::bind("127.0.0.1:0", quick(), move |req| {
        Response::text(200, host_allowed(&names, req).to_string()).into()
    })
    .unwrap();
    let r = send(
        s.local_addr(),
        b"GET / HTTP/1.1\r\nHost: node.example.net:8443\r\n\r\n",
    );
    assert!(r.ends_with("true"), "{r}");
    let r = send(
        s.local_addr(),
        b"GET / HTTP/1.1\r\nHost: other.example\r\n\r\n",
    );
    assert!(r.ends_with("false"), "{r}");
    s.stop();
}
