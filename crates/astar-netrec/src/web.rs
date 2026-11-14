// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The recordings pages on the recorder's own loopback port.
//!
//! This is the only way in, so there is no sign-in: whoever reaches the port
//! is on this machine and is treated as the admin, private sessions and
//! delete included. Two checks keep a web page in the user's browser from
//! using that: the `Host` must name this port on a loopback name (a
//! DNS-rebinding page arrives with its own name), and a delete must be a
//! same-origin JSON post.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use astar_http::guards::{is_json, origin_rule};
use astar_http::{Limits, Reply, Request, Response, Server, ServerOptions};
use astar_netarchive::{Access, NETS_CSP, NetsRequest, Pages, ViewOpts};

use crate::config::Config;

/// Where the page's delete button posts.
const DELETE_PATH: &str = "/nets/delete";

/// Serve the pages on `[web] bind`, when `[web] local` is on.
///
/// # Errors
/// The bind failed. The caller logs it and carries on recording.
pub fn serve_local(cfg: &Config) -> std::io::Result<Option<Server>> {
    if !cfg.web.local {
        return Ok(None);
    }
    let pages = Arc::new(Pages::new(
        cfg.recorder.root.clone(),
        ViewOpts {
            delete_url: Some(DELETE_PATH.into()),
            sign_in_url: None,
            fallback_my_calls: cfg.recorder.my_callsigns.clone(),
        },
    ));
    // The port in `Host` is the one actually bound, which differs from the
    // configured one only for port 0 (the tests').
    let port = Arc::new(AtomicU16::new(cfg.web.bind.port()));
    let seen = Arc::clone(&port);
    let server = Server::bind_with(
        &cfg.web.bind.to_string(),
        Limits::loopback(),
        ServerOptions {
            name: "netrec-web",
            csp: Some(NETS_CSP),
            ..ServerOptions::default()
        },
        move |req: &mut Request<'_>| respond(&pages, seen.load(Ordering::Relaxed), req),
    )?;
    port.store(server.local_addr().port(), Ordering::Relaxed);
    Ok(Some(server))
}

/// Whether `host` (the raw header) is a loopback name with this port.
fn host_ok(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return false };
    let host = host.trim().to_ascii_lowercase();
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
    .contains(&host)
}

fn respond(pages: &Pages, port: u16, req: &mut Request<'_>) -> Reply {
    if !host_ok(req.header("Host"), port) {
        return Response::text(421, "misdirected request").into();
    }
    let (method, path) = (req.method().to_owned(), req.path().to_owned());
    if method == "POST" && path == DELETE_PATH {
        if let Err(refused) = origin_rule(req) {
            return Response::text(refused.status(), "refused").into();
        }
        if !is_json(req.header("Content-Type")) {
            return Response::text(415, "expected application/json").into();
        }
        return match req.body() {
            Ok(body) => pages.delete(body, Access::Admin).into(),
            Err(e) => Response::text(e.status, "refused").into(),
        };
    }
    if Pages::is_nets_path(&path) {
        let range = req.header("Range").map(str::to_owned);
        return pages
            .handle(&NetsRequest {
                method: &method,
                path: &path,
                access: Access::Admin,
                offer_sign_in: false,
            })
            .into_reply(range);
    }
    if path == "/" {
        return Response::new(303).header("Location", "/nets/").into();
    }
    Response::text(404, "not found").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::from_toml_str;
    use astar_netarchive::fixture::{Fixture, write_fixture};
    use std::fmt::Write as _;
    use std::io::{Read, Write as _};
    use std::net::{SocketAddr, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    const THURSDAY: &str = "thursday-d-star-net";

    struct Rig {
        root: PathBuf,
        f: Fixture,
        server: Option<Server>,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            drop(self.server.take());
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Rig {
        fn addr(&self) -> SocketAddr {
            self.server.as_ref().unwrap().local_addr()
        }
    }

    fn rig(local: bool) -> Rig {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "netrec-web-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let f = write_fixture(&root);
        let toml = format!(
            "[recorder]\nroot = {:?}\ncallsign = \"N0CALL\"\nmy_callsigns = [\"N0CALL\"]\n\n[web]\nlocal = {local}\nbind = \"127.0.0.1:0\"\n",
            root.display().to_string()
        );
        let cfg = from_toml_str(&toml).unwrap();
        let server = serve_local(&cfg).unwrap();
        Rig { root, f, server }
    }

    /// One request over a real socket: the status and the body.
    fn fetch(
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String) {
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut req = format!("{method} {path} HTTP/1.1\r\n");
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            write!(req, "Host: 127.0.0.1:{}\r\n", addr.port()).unwrap();
        }
        for (k, v) in headers {
            write!(req, "{k}: {v}\r\n").unwrap();
        }
        write!(
            req,
            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut all = Vec::new();
        let _ = s.read_to_end(&mut all);
        let text = String::from_utf8_lossy(&all).into_owned();
        let status = text
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = text
            .split_once("\r\n\r\n")
            .map_or(String::new(), |(_, b)| b.to_owned());
        (status, body)
    }

    fn delete_body(net: &str, session: &str) -> String {
        format!(r#"{{"net":"{net}","session":"{session}"}}"#)
    }

    #[test]
    fn local_server_refuses_a_foreign_host() {
        let r = rig(true);
        let a = r.addr();
        for host in [
            "evil.example",
            "evil.example:80",
            "127.0.0.1",
            "127.0.0.1:1",
            "localhost.evil.example",
        ] {
            let (st, _) = fetch(a, "GET", "/nets/index.json", &[("Host", host)], "");
            assert_eq!(st, 421, "{host}");
        }
        for host in [
            format!("127.0.0.1:{}", a.port()),
            format!("localhost:{}", a.port()),
            format!("LocalHost:{}", a.port()),
        ] {
            let (st, _) = fetch(a, "GET", "/nets/index.json", &[("Host", &host)], "");
            assert_eq!(st, 200, "{host}");
        }
    }

    #[test]
    fn local_server_shows_private_sessions_without_sign_in() {
        let r = rig(true);
        let (st, body) = fetch(r.addr(), "GET", "/nets/index.json", &[], "");
        assert_eq!(st, 200);
        assert!(body.contains(THURSDAY), "{body}");
        let (st, _) = fetch(
            r.addr(),
            "GET",
            &format!("/nets/{THURSDAY}/{}/data.json", r.f.private_session),
            &[],
            "",
        );
        assert_eq!(st, 200);
        let (st, _) = fetch(r.addr(), "GET", "/", &[], "");
        assert_eq!(st, 303);
        let (st, _) = fetch(r.addr(), "GET", "/elsewhere", &[], "");
        assert_eq!(st, 404);
    }

    #[test]
    fn delete_needs_same_origin_json() {
        let r = rig(true);
        let a = r.addr();
        let host = format!("127.0.0.1:{}", a.port());
        let origin = format!("http://{host}");
        let body = delete_body(THURSDAY, &r.f.private_session);
        let json = ("Content-Type", "application/json");
        let archive = astar_netarchive::Archive::new(r.root.clone());
        let still_there = || archive.find(THURSDAY, &r.f.private_session).is_some();

        let (st, _) = fetch(
            a,
            "POST",
            DELETE_PATH,
            &[json, ("Origin", "http://evil.example")],
            &body,
        );
        assert_eq!(st, 403);
        let (st, _) = fetch(
            a,
            "POST",
            DELETE_PATH,
            &[("Content-Type", "text/plain"), ("Origin", &origin)],
            &body,
        );
        assert_eq!(st, 415);
        let (st, _) = fetch(
            a,
            "POST",
            DELETE_PATH,
            &[("Content-Type", "text/plain")],
            &body,
        );
        assert_eq!(st, 415);
        assert!(still_there());

        let (st, _) = fetch(
            a,
            "POST",
            DELETE_PATH,
            &[json, ("Origin", &origin)],
            &delete_body(THURSDAY, &r.f.open_session),
        );
        assert_eq!(st, 409);

        let (st, b) = fetch(a, "POST", DELETE_PATH, &[json, ("Origin", &origin)], &body);
        assert_eq!(st, 200, "{b}");
        assert!(!still_there());
    }

    #[test]
    fn web_off_serves_nothing() {
        let r = rig(false);
        assert!(r.server.is_none());
    }

    #[test]
    fn old_sessions_use_the_config_my_callsigns() {
        let r = rig(true);
        let (st, body) = fetch(
            r.addr(),
            "GET",
            &format!("/nets/{THURSDAY}/{}/data.json", r.f.private_session),
            &[],
            "",
        );
        assert_eq!(st, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["my_calls"], serde_json::json!(["N0CALL"]));
    }
}
