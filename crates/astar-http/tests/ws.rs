// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#![cfg(feature = "ws")]
mod support;
use astar_http::ws::{self, tungstenite};
use astar_http::{Limits, Reply, Response, Server, ServerOptions, StreamClass};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{quick, send};
use tungstenite::Message;

fn connect(addr: SocketAddr) -> tungstenite::WebSocket<TcpStream> {
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (ws, resp) = tungstenite::client::client(format!("ws://{addr}/ws"), tcp).unwrap();
    assert_eq!(resp.status(), 101);
    ws
}

#[test]
fn an_upgrade_gets_a_websocket_and_a_plain_get_gets_400() {
    let s = Server::bind("127.0.0.1:0", quick(), |req| {
        if req.path() != "/ws" {
            return Response::text(404, "no\n").into();
        }
        Reply::WebSocket {
            class: StreamClass::Counted,
            config: None,
            run: Box::new(|mut ws| {
                let _ = ws.send(Message::text("hello"));
                let _ = ws.close(None);
                let _ = ws.flush();
            }),
        }
    })
    .unwrap();
    let mut c = connect(s.local_addr());
    assert_eq!(c.read().unwrap(), Message::text("hello"));

    let plain = send(s.local_addr(), b"GET /ws HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(plain.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{plain}");
    assert!(
        plain.ends_with("\r\n\r\nWebSocket upgrade expected\n"),
        "{plain}"
    );
    s.stop();
}

#[test]
fn a_websocket_takes_a_stream_slot() {
    let s = Server::bind(
        "127.0.0.1:0",
        Limits {
            streams_per_client: 1,
            ..quick()
        },
        |_| Reply::WebSocket {
            class: StreamClass::Counted,
            config: None,
            run: Box::new(|mut ws| {
                // Held until the client goes.
                while !matches!(ws.read(), Err(e) if !ws::is_quiet(&e)) {}
            }),
        },
    )
    .unwrap();
    let held = connect(s.local_addr());
    let tcp = TcpStream::connect(s.local_addr()).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let refused = tungstenite::client::client(format!("ws://{}/ws", s.local_addr()), tcp);
    match refused {
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r))) => {
            assert_eq!(r.status(), 429);
        }
        other => panic!("a second stream from one client: {:?}", other.map(|_| ())),
    }
    drop(held);
    s.stop();
}

/// A server whose WebSocket sends `SENT` messages, listening for a close
/// for longer than the write stall between each, and reports how it ended.
const SENT: usize = 6;
fn idle_server(options: ServerOptions) -> (Server, Arc<Mutex<Option<String>>>) {
    // Every read the stream makes waits at most the write stall, then fails
    // with WouldBlock / TimedOut: that is "nothing yet", not "gone".
    let ended = Arc::new(Mutex::new(None::<String>));
    let e2 = Arc::clone(&ended);
    let s = Server::bind_with(
        "127.0.0.1:0",
        Limits {
            write_stall: Duration::from_millis(100),
            ..quick()
        },
        options,
        move |_| {
            let e3 = Arc::clone(&e2);
            Reply::WebSocket {
                class: StreamClass::Counted,
                config: None,
                run: Box::new(move |mut ws| {
                    let mut why = "sent everything".to_string();
                    'out: for i in 0..SENT {
                        if let Err(e) = ws.send(Message::text(i.to_string())) {
                            why = format!("send: {e}");
                            break;
                        }
                        for _ in 0..2 {
                            match ws.read() {
                                Err(e) if ws::is_quiet(&e) => {}
                                Err(e) => {
                                    why = format!("read: {e}");
                                    break 'out;
                                }
                                Ok(m) => {
                                    why = format!("got {m:?}");
                                    break 'out;
                                }
                            }
                        }
                    }
                    // Before the close, so the client sees it once it has read that.
                    *e3.lock().unwrap() = Some(why);
                    let _ = ws.close(None);
                    let _ = ws.flush();
                }),
            }
        },
    )
    .unwrap();
    (s, ended)
}

/// Read every text message until the close; the client sends nothing for
/// the whole run (well over the stall).
fn read_all<S: Read + Write>(c: &mut tungstenite::WebSocket<S>) -> Vec<String> {
    let mut got = Vec::new();
    loop {
        match c.read() {
            Ok(Message::Text(t)) => got.push(t.to_string()),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    got
}

#[test]
fn an_idle_client_is_not_dropped_while_the_server_sends() {
    let (s, ended) = idle_server(ServerOptions::default());
    let got = read_all(&mut connect(s.local_addr()));
    let want: Vec<String> = (0..SENT).map(|i| i.to_string()).collect();
    assert_eq!(got, want);
    assert_eq!(ended.lock().unwrap().as_deref(), Some("sent everything"));
    s.stop();
}

#[cfg(feature = "tls")]
#[test]
fn an_idle_client_over_tls_is_not_dropped_while_the_server_sends() {
    // The same, through rustls: its stream must surface the per-read bound
    // as errors `ws::is_quiet` recognises.
    use rustls_pki_types::ServerName;
    let (key, root) = support::tls::self_signed("test.invalid");
    let cfg = astar_http::TlsConfig::new();
    cfg.install(key);
    let (s, ended) = idle_server(ServerOptions {
        tls: Some(cfg),
        ..ServerOptions::default()
    });
    let name = ServerName::try_from("test.invalid").unwrap();
    let conn = rustls::ClientConnection::new(support::tls::client_config(&[&root]), name).unwrap();
    let tcp = TcpStream::connect(s.local_addr()).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let tls = rustls::StreamOwned::new(conn, tcp);
    let (mut c, resp) =
        tungstenite::client::client(format!("ws://{}/ws", s.local_addr()), tls).unwrap();
    assert_eq!(resp.status(), 101);
    let got = read_all(&mut c);
    let want: Vec<String> = (0..SENT).map(|i| i.to_string()).collect();
    assert_eq!(got, want);
    assert_eq!(ended.lock().unwrap().as_deref(), Some("sent everything"));
    s.stop();
}

/// A server whose one-slot WebSocket reads one message and reports what
/// the read returned.
fn reading_server(config: Option<ws::WebSocketConfig>) -> (Server, Arc<Mutex<Option<String>>>) {
    let seen = Arc::new(Mutex::new(None::<String>));
    let s2 = Arc::clone(&seen);
    let s = Server::bind(
        "127.0.0.1:0",
        Limits {
            streams_per_client: 1,
            ..quick()
        },
        move |_| {
            let s3 = Arc::clone(&s2);
            Reply::WebSocket {
                class: StreamClass::Counted,
                config,
                run: Box::new(move |mut ws| {
                    let r = loop {
                        match ws.read() {
                            Err(e) if ws::is_quiet(&e) => {}
                            Err(tungstenite::Error::Capacity(e)) => break format!("capacity: {e}"),
                            Err(e) => break format!("error: {e}"),
                            Ok(Message::Binary(b)) => break format!("binary {}", b.len()),
                            Ok(m) => break format!("got {m:?}"),
                        }
                    };
                    *s3.lock().unwrap() = Some(r);
                }),
            }
        },
    )
    .unwrap();
    (s, seen)
}

fn wait_seen(seen: &Mutex<Option<String>>) -> Option<String> {
    let t = std::time::Instant::now();
    while seen.lock().unwrap().is_none() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    seen.lock().unwrap().clone()
}

#[test]
fn a_frame_over_the_default_limit_is_refused_and_frees_the_slot() {
    let (s, seen) = reading_server(None);
    let mut c = connect(s.local_addr());
    let _ = c.send(Message::binary(vec![7u8; 64 * 1024 + 1]));
    let r = wait_seen(&seen);
    assert!(
        r.as_deref().is_some_and(|r| r.starts_with("capacity:")),
        "{r:?}"
    );
    // The server closed the connection.
    loop {
        match c.read() {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    // And freed the one slot: a second WebSocket from this client opens.
    let _again = connect(s.local_addr());
    s.stop();
}

#[test]
fn a_caller_can_raise_the_limit() {
    let cfg = ws::default_config()
        .max_message_size(Some(128 * 1024))
        .max_frame_size(Some(128 * 1024));
    let (s, seen) = reading_server(Some(cfg));
    let mut c = connect(s.local_addr());
    c.send(Message::binary(vec![7u8; 64 * 1024 + 1])).unwrap();
    assert_eq!(wait_seen(&seen).as_deref(), Some("binary 65537"));
    s.stop();
}

#[test]
fn a_stream_can_upgrade_after_deciding_to() {
    // A Reply::Stream that checks something first (a hub with room) and
    // then upgrades: the accept key comes from the request, the 101 and
    // the socket from ws::upgrade.
    let s = Server::bind("127.0.0.1:0", quick(), |req| {
        let Some(accept) = ws::accept_key(req) else {
            return Response::text(400, "no\n").into();
        };
        Reply::Stream {
            class: StreamClass::PerClientOnly,
            run: Box::new(move |conn| {
                let Ok(mut ws) = ws::upgrade(conn, &accept, None) else {
                    return;
                };
                let _ = ws.send(Message::text("upgraded"));
                let _ = ws.close(None);
                let _ = ws.flush();
            }),
        }
    })
    .unwrap();
    // tungstenite's client checks Sec-WebSocket-Accept against its key.
    let mut c = connect(s.local_addr());
    assert_eq!(c.read().unwrap(), Message::text("upgraded"));
    let plain = send(s.local_addr(), b"GET /ws HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(plain.starts_with("HTTP/1.1 400 "), "{plain}");
    s.stop();
}
