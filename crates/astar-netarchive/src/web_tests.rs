// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Tests for the `/nets/` routes.

use super::*;
use crate::fixture::{Fixture, write_fixture};
use crate::log::read_rows;
use crate::model::LogKind;
use astar_http::{Limits, Server};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const THURSDAY: &str = "thursday-d-star-net";

struct Setup {
    root: PathBuf,
    f: Fixture,
    pages: Arc<Pages>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn temp(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "netarchive-web-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn setup(tag: &str) -> Setup {
    let root = temp(tag);
    let f = write_fixture(&root);
    let opts = ViewOpts {
        delete_url: Some("/nets/delete".into()),
        sign_in_url: Some("/signin".into()),
        fallback_my_calls: Vec::new(),
    };
    let pages = Arc::new(Pages::new(root.clone(), opts));
    Setup { root, f, pages }
}

fn handle(p: &Pages, access: Access, method: &str, path: &str) -> NetsReply {
    p.handle(&NetsRequest {
        method,
        path,
        access,
        offer_sign_in: true,
    })
}

fn get(p: &Pages, access: Access, path: &str) -> Response {
    match handle(p, access, "GET", path) {
        NetsReply::Response(r) => r,
        NetsReply::File { path, .. } => panic!("a file, {}", path.display()),
    }
}

fn header<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The first over file of a session, as the log names it, percent-encoded.
fn first_over(s: &Setup, net: &str, session: &str) -> (String, PathBuf) {
    let e = s.pages.archive().find(net, session).unwrap();
    let rows = read_rows(&e.dir).unwrap();
    let file = rows
        .iter()
        .find_map(|r| match &r.kind {
            LogKind::Over { file, .. } => Some(file.clone()),
            _ => None,
        })
        .unwrap();
    (crate::view::encode(&file), e.dir.join("overs").join(&file))
}

/// A server that answers `/nets/` as `pages` would, with the access level
/// named in an `X-Test-Access` header (anonymous without one).
fn serve(pages: &Arc<Pages>) -> Server {
    let pages = Arc::clone(pages);
    Server::bind("127.0.0.1:0", Limits::loopback(), move |req| {
        let access = match req.header("x-test-access") {
            Some("viewer") => Access::Viewer,
            Some("admin") => Access::Admin,
            _ => Access::Anonymous,
        };
        let range = req.header("range").map(str::to_owned);
        pages
            .handle(&NetsRequest {
                method: req.method(),
                path: req.path(),
                access,
                offer_sign_in: true,
            })
            .into_reply(range)
    })
    .unwrap()
}

/// One request over a real socket; the head and the raw body.
fn fetch(addr: SocketAddr, method: &str, path: &str, extra: &str) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut all = Vec::new();
    let _ = s.read_to_end(&mut all);
    let i = all
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a head");
    (
        String::from_utf8_lossy(&all[..i + 4]).into_owned(),
        all[i + 4..].to_vec(),
    )
}

fn status(head: &str) -> u16 {
    head.split(' ').nth(1).unwrap().parse().unwrap()
}

/// Every route that names a session, for `net`/`session`.
fn session_routes(net: &str, session: &str, over: &str) -> Vec<String> {
    let base = format!("/nets/{net}/{session}");
    vec![
        base.clone(),
        format!("{base}/"),
        format!("{base}/data.json"),
        format!("{base}/session.flac"),
        format!("{base}/session.m4a"),
        format!("{base}/overs/{over}"),
    ]
}

#[test]
fn private_session_every_url_is_404_for_anonymous() {
    let s = setup("anon404");
    let (over, _) = first_over(&s, THURSDAY, &s.f.private_session);
    let base = format!("/nets/{THURSDAY}/{}", s.f.private_session);
    let missing_json = get(&s.pages, Access::Anonymous, "/nets/nope/nope/data.json");
    assert_eq!(missing_json.status, 404);
    assert_eq!(missing_json.body, br#"{"error":"no such session"}"#);
    for path in [
        format!("{base}/data.json"),
        format!("{base}/session.flac"),
        format!("{base}/overs/{over}"),
    ] {
        assert_eq!(
            get(&s.pages, Access::Anonymous, &path),
            missing_json,
            "{path}"
        );
    }
    let missing_page = get(&s.pages, Access::Anonymous, "/nets/nope/nope/");
    assert_eq!(missing_page.status, 404);
    assert_eq!(
        get(&s.pages, Access::Anonymous, &format!("{base}/")),
        missing_page
    );
    // The index does not name it either.
    let list = get(&s.pages, Access::Anonymous, "/nets/index.json");
    let text = String::from_utf8(list.body).unwrap();
    assert!(!text.contains("Thursday"), "{text}");
}

#[test]
fn anonymous_gets_404_on_every_private_route() {
    let s = setup("anonroutes");
    let server = serve(&s.pages);
    let addr = server.local_addr();
    let (over, _) = first_over(&s, THURSDAY, &s.f.private_session);
    let private = session_routes(THURSDAY, &s.f.private_session, &over);
    let missing = session_routes("nope", "nope", &over);
    for (p, m) in private.iter().zip(&missing) {
        for method in ["GET", "HEAD"] {
            let (ph, pb) = fetch(addr, method, p, "");
            let (mh, mb) = fetch(addr, method, m, "");
            assert_eq!(status(&ph), 404, "{method} {p}: {ph}");
            assert_eq!(ph, mh, "{method} {p}");
            assert_eq!(pb, mb, "{method} {p}");
        }
    }
    // The open session is private too.
    let (h, _) = fetch(
        addr,
        "GET",
        &format!("/nets/{THURSDAY}/{}/data.json", s.f.open_session),
        "",
    );
    assert_eq!(status(&h), 404, "{h}");
    // Delete is refused alike for a hidden session and a missing one.
    let hidden = s.pages.delete(
        format!(
            r#"{{"net":"{THURSDAY}","session":"{}"}}"#,
            s.f.private_session
        )
        .as_bytes(),
        Access::Anonymous,
    );
    let nope = s
        .pages
        .delete(br#"{"net":"nope","session":"nope"}"#, Access::Anonymous);
    assert_eq!(hidden, nope);
    assert_eq!(hidden.status, 403);
    server.stop();
}

#[test]
fn signed_in_viewer_gets_every_private_route() {
    let s = setup("viewerroutes");
    let server = serve(&s.pages);
    let addr = server.local_addr();
    let (over, _) = first_over(&s, THURSDAY, &s.f.private_session);
    let routes = session_routes(THURSDAY, &s.f.private_session, &over);
    let want = [308, 200, 200, 200, 404, 200];
    for (path, want) in routes.iter().zip(want) {
        for access in ["viewer", "admin"] {
            let (h, b) = fetch(addr, "GET", path, &format!("X-Test-Access: {access}\r\n"));
            assert_eq!(status(&h), want, "{access} {path}: {h}");
            if want == 200 {
                assert!(!b.is_empty(), "{access} {path}");
            }
        }
    }
    let (h, b) = fetch(
        addr,
        "GET",
        &format!("/nets/{THURSDAY}/{}/data.json", s.f.private_session),
        "X-Test-Access: viewer\r\n",
    );
    assert!(h.contains("Content-Type: application/json\r\n"), "{h}");
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["net"], "Thursday D-Star net");
    // The peaks travel inside data.json, computed for a session without
    // peaks.json.
    assert_eq!(v["items"][1]["peaks"].as_array().unwrap().len(), 48);
    server.stop();
}

/// A bind without sign-in passes `offer_sign_in: false`, and the list then
/// names no sign-in URL even though the pages were built with one.
#[test]
fn a_bind_without_sign_in_offers_no_sign_in_url() {
    let s = setup("no-sign-in");
    let list = |offer_sign_in| match s.pages.handle(&NetsRequest {
        method: "GET",
        path: "/nets/index.json",
        access: Access::Anonymous,
        offer_sign_in,
    }) {
        NetsReply::Response(r) => String::from_utf8(r.body).unwrap(),
        NetsReply::File { .. } => panic!("a file"),
    };
    assert!(list(true).contains(r#""sign_in_url":"/signin""#));
    assert!(
        list(false).contains(r#""sign_in_url":null"#),
        "{}",
        list(false)
    );
}

#[test]
fn viewer_sees_private_sessions_admin_alone_deletes() {
    let s = setup("delete");
    let base = format!("/nets/{THURSDAY}/{}", s.f.private_session);
    assert_eq!(
        get(&s.pages, Access::Viewer, &format!("{base}/")).status,
        200
    );
    assert_eq!(
        get(&s.pages, Access::Viewer, &format!("{base}/data.json")).status,
        200
    );
    let body = format!(
        r#"{{"net":"{THURSDAY}","session":"{}"}}"#,
        s.f.private_session
    );
    let r = s.pages.delete(body.as_bytes(), Access::Viewer);
    assert_eq!(r.status, 403);
    let dir = s.root.join(THURSDAY).join("2026-11-05 1900");
    assert!(dir.is_dir());
    let r = s.pages.delete(body.as_bytes(), Access::Admin);
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.body, br#"{"deleted":true}"#);
    assert_eq!(header(&r, "Cache-Control"), Some("no-store"));
    assert!(!dir.exists());
    // Gone now: a second delete and the page are both 404.
    assert_eq!(s.pages.delete(body.as_bytes(), Access::Admin).status, 404);
    assert_eq!(
        get(&s.pages, Access::Admin, &format!("{base}/data.json")).status,
        404
    );
}

#[test]
fn delete_refuses_the_open_session_with_409() {
    let s = setup("open409");
    let body = format!(r#"{{"net":"{THURSDAY}","session":"{}"}}"#, s.f.open_session);
    let r = s.pages.delete(body.as_bytes(), Access::Admin);
    assert_eq!(r.status, 409);
    assert_eq!(r.body, br#"{"error":"that session is still open"}"#);
    assert!(s.root.join(THURSDAY).join("2026-11-07 1900").is_dir());
}

#[test]
fn delete_refuses_a_bad_body_with_400() {
    let s = setup("bad400");
    for bad in [
        &b""[..],
        b"{",
        b"[]",
        br#"{"net":"x"}"#,
        br#"{"net":1,"session":2}"#,
    ] {
        let r = s.pages.delete(bad, Access::Admin);
        assert_eq!(r.status, 400, "{}", String::from_utf8_lossy(bad));
    }
    // A traversal in the body is just a session that does not exist.
    let r = s
        .pages
        .delete(br#"{"net":"..","session":".."}"#, Access::Admin);
    assert_eq!(r.status, 404);
    assert!(s.root.is_dir());
}

#[test]
fn paths_never_leave_the_index() {
    let s = setup("paths");
    let missing = get(&s.pages, Access::Admin, "/nets/nope/nope/data.json");
    let pub_base = format!("/nets/{}/{}", s.f.public_net, s.f.public_session);
    for path in [
        "/nets/x/../../etc/passwd".to_owned(),
        "/nets/%2e%2e/%2e%2e/data.json".to_owned(),
        "/nets/%2e%2e/%2e%2e/session.flac".to_owned(),
        format!("{pub_base}/overs/..%2Fsession.json"),
        format!("{pub_base}/overs/%2e%2e%2f%2e%2e%2fsession.json"),
        format!("{pub_base}/overs/999%2019-00-00%20N0CALL.flac"),
        format!("{pub_base}/overs/"),
        format!("{pub_base}/session.json"),
        format!("{pub_base}/log.jsonl"),
    ] {
        let r = get(&s.pages, Access::Admin, &path);
        assert_eq!(r.status, 404, "{path}");
        assert_ne!(r.body, b"", "{path}");
    }
    assert_eq!(
        get(
            &s.pages,
            Access::Admin,
            &format!("{pub_base}/overs/not-in-the-log.flac")
        ),
        missing
    );

    // An over file that is a symlink to a file outside the archive.
    let (over, over_path) = first_over(&s, &s.f.public_net, &s.f.public_session);
    let outside = temp("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let secret = outside.join("secret.flac");
    std::fs::write(&secret, b"not for the web").unwrap();
    std::fs::remove_file(&over_path).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, &over_path).unwrap();
    let server = serve(&s.pages);
    let (h, b) = fetch(
        server.local_addr(),
        "GET",
        &format!("{pub_base}/overs/{over}"),
        "",
    );
    assert_eq!(status(&h), 404, "{h}");
    assert_eq!(b, br#"{"error":"no such session"}"#);
    server.stop();
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn a_file_swapped_for_a_symlink_after_the_check_is_not_served() {
    let s = setup("swap");
    let path = format!(
        "/nets/{}/{}/session.flac",
        s.f.public_net, s.f.public_session
    );
    let reply = handle(&s.pages, Access::Anonymous, "GET", &path);
    let NetsReply::File { path: file, mime } = &reply else {
        panic!("{reply:?}")
    };
    assert_eq!(*mime, "audio/flac");
    // Swap the file for a link between the route and the stream.
    let outside = temp("swap-outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x"), b"secret").unwrap();
    std::fs::remove_file(file).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("x"), file).unwrap();
    let reply = Arc::new(std::sync::Mutex::new(Some(reply)));
    let server = Server::bind("127.0.0.1:0", Limits::loopback(), move |_req| {
        reply.lock().unwrap().take().unwrap().into_reply(None)
    })
    .unwrap();
    let (h, b) = fetch(server.local_addr(), "GET", "/x", "");
    assert_eq!(status(&h), 404, "{h}");
    assert_eq!(b, br#"{"error":"no such session"}"#);
    server.stop();
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn session_audio_answers_ranges() {
    let s = setup("ranges");
    let server = serve(&s.pages);
    let addr = server.local_addr();
    let path = format!(
        "/nets/{}/{}/session.flac",
        s.f.public_net, s.f.public_session
    );
    let whole = std::fs::read(
        s.root
            .join("tuesday-m17-net")
            .join("2026-11-03 1855")
            .join("session.flac"),
    )
    .unwrap();
    let (h, b) = fetch(addr, "GET", &path, "Range: bytes=0-99\r\n");
    assert_eq!(status(&h), 206, "{h}");
    assert_eq!(b, &whole[..100]);
    assert!(h.contains("Content-Type: audio/flac\r\n"), "{h}");
    assert!(h.contains("Cache-Control: no-store\r\n"), "{h}");
    let (h, _) = fetch(addr, "GET", &path, "Range: bytes=999999999-\r\n");
    assert_eq!(status(&h), 416, "{h}");
    let (h, b) = fetch(addr, "GET", &path, "");
    assert_eq!(status(&h), 200, "{h}");
    assert_eq!(b, whole);
    let (h, b) = fetch(addr, "HEAD", &path, "");
    assert_eq!(status(&h), 200, "{h}");
    assert_eq!(b, Vec::<u8>::new());
    // An over file, by its log name.
    let (over, over_path) = first_over(&s, &s.f.public_net, &s.f.public_session);
    let (h, b) = fetch(
        addr,
        "GET",
        &format!(
            "/nets/{}/{}/overs/{over}",
            s.f.public_net, s.f.public_session
        ),
        "",
    );
    assert_eq!(status(&h), 200, "{h}");
    assert_eq!(b, std::fs::read(over_path).unwrap());
    server.stop();
}

#[test]
fn only_get_and_head() {
    let s = setup("methods");
    for method in ["POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
        for path in ["/nets/", "/nets/index.json", "/nets/nope/nope/data.json"] {
            let NetsReply::Response(r) = handle(&s.pages, Access::Admin, method, path) else {
                panic!("{method} {path}")
            };
            assert_eq!(r.status, 405, "{method} {path}");
            assert_eq!(header(&r, "Allow"), Some("GET, HEAD"));
        }
    }
    let NetsReply::Response(r) = handle(&s.pages, Access::Anonymous, "HEAD", "/nets/index.json")
    else {
        panic!()
    };
    assert_eq!(r.status, 200);
}

#[test]
fn responses_carry_the_csp_and_nosniff() {
    let s = setup("csp");
    let base = format!("/nets/{}/{}", s.f.public_net, s.f.public_session);
    let mut seen = Vec::new();
    for path in [
        "/nets".to_owned(),
        "/nets/".to_owned(),
        "/nets/nets.css".to_owned(),
        "/nets/list.js".to_owned(),
        "/nets/session.js".to_owned(),
        "/nets/index.json".to_owned(),
        "/nets/whatever".to_owned(),
        "/nets/nope/nope/data.json".to_owned(),
        base.clone(),
        format!("{base}/"),
        format!("{base}/data.json"),
    ] {
        let r = get(&s.pages, Access::Anonymous, &path);
        seen.push(r.status);
        assert_eq!(
            header(&r, "Content-Security-Policy"),
            Some(NETS_CSP),
            "{path}"
        );
        assert_eq!(header(&r, "X-Content-Type-Options"), Some("nosniff"));
    }
    assert_eq!(
        seen,
        [308, 200, 200, 200, 200, 200, 404, 404, 308, 200, 200]
    );
    let NetsReply::Response(r) = handle(&s.pages, Access::Anonymous, "POST", "/nets/") else {
        panic!()
    };
    assert_eq!(header(&r, "Content-Security-Policy"), Some(NETS_CSP));
    for r in [
        s.pages.delete(b"x", Access::Admin),
        s.pages.delete(b"{}", Access::Viewer),
    ] {
        assert_eq!(header(&r, "Content-Security-Policy"), Some(NETS_CSP));
        assert_eq!(header(&r, "X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(header(&r, "Cache-Control"), Some("no-store"));
    }
    // JSON is never cached; assets revalidate.
    let r = get(&s.pages, Access::Anonymous, "/nets/index.json");
    assert_eq!(header(&r, "Cache-Control"), Some("no-store"));
    assert_eq!(header(&r, "Content-Type"), Some("application/json"));
    let r = get(&s.pages, Access::Anonymous, "/nets/nets.css");
    assert_eq!(header(&r, "Cache-Control"), Some("no-cache"));
    assert_eq!(header(&r, "Content-Type"), Some("text/css; charset=utf-8"));
    let r = get(&s.pages, Access::Anonymous, "/nets/list.js");
    assert_eq!(
        header(&r, "Content-Type"),
        Some("text/javascript; charset=utf-8")
    );
    // And a file streamed through a server.
    let server = serve(&s.pages);
    let (h, _) = fetch(
        server.local_addr(),
        "HEAD",
        &format!("{base}/session.flac"),
        "",
    );
    assert!(
        h.contains(&format!("Content-Security-Policy: {NETS_CSP}\r\n")),
        "{h}"
    );
    assert!(h.contains("X-Content-Type-Options: nosniff\r\n"), "{h}");
    server.stop();
}

#[test]
fn trailing_slash_redirects() {
    let s = setup("slash");
    let r = get(&s.pages, Access::Anonymous, "/nets");
    assert_eq!(r.status, 308);
    assert_eq!(header(&r, "Location"), Some("/nets/"));
    let base = format!("/nets/{}/{}", s.f.public_net, s.f.public_session);
    let r = get(&s.pages, Access::Anonymous, &base);
    assert_eq!(r.status, 308);
    assert_eq!(header(&r, "Location"), Some(format!("{base}/").as_str()));
    // The folder's own name redirects to the slug.
    let r = get(
        &s.pages,
        Access::Anonymous,
        &format!("/nets/{}/2026-11-03%201855", s.f.public_net),
    );
    assert_eq!(header(&r, "Location"), Some(format!("{base}/").as_str()));
    let r = get(&s.pages, Access::Anonymous, &format!("{base}/"));
    assert_eq!(r.status, 200);
    assert_eq!(header(&r, "Content-Type"), Some("text/html; charset=utf-8"));
}

#[test]
fn is_nets_path_names_the_prefix_only() {
    for yes in ["/nets", "/nets/", "/nets/index.json", "/nets/a/b/"] {
        assert!(Pages::is_nets_path(yes), "{yes}");
    }
    for no in ["/", "/net", "/netsx", "/nets.css", "/other/nets/"] {
        assert!(!Pages::is_nets_path(no), "{no}");
    }
}

// ---- the page assets ------------------------------------------------------

const TOKEN_MARKER: &str = "/* ---- end of tokens ---- */";

/// Every colour lives in the two token blocks (dark, then light), and the
/// rules below the marker name none.
#[test]
fn nets_css_tokens_have_dark_and_light() {
    let (tokens, rest) = NETS_CSS
        .split_once(TOKEN_MARKER)
        .expect("the token blocks end at the marker comment");
    assert!(tokens.contains("@media (prefers-color-scheme: light)"));
    for name in [
        // The shared set, the same values as astar-server's style.css.
        "--bg",
        "--text",
        "--text-dim",
        "--card",
        "--card-shadow",
        "--edge",
        "--edge-glow",
        "--edge-hover",
        "--row-edge",
        "--btn-bg",
        "--btn-hover",
        "--banner-bg",
        "--err-text",
        "--glow-a",
        "--glow-b",
        "--glow-c",
        "--green",
        "--amber",
        "--red",
        "--green-rgb",
        "--amber-rgb",
        "--red-rgb",
        // The page's own.
        "--v1",
        "--v2",
        "--v3",
        "--v-rgb",
        "--mag-lt",
        "--ring-end",
        "--me-a",
        "--me-b",
        "--me-rgb",
        "--orb-ink",
        "--orb-shade",
        "--on-color",
        "--sheen",
    ] {
        let decl = format!("{name}:");
        assert_eq!(
            tokens.matches(&decl).count(),
            2,
            "{name} is declared once for dark and once for light"
        );
    }
    assert_eq!(
        astar_http::testing::colour_literals(rest),
        Vec::<String>::new(),
        "every colour lives in the token blocks; these literals are below the marker"
    );
}

/// Callsigns come off the air. No page script may hand a string to a sink
/// that parses HTML.
#[test]
fn scripts_never_build_markup() {
    for (name, js) in [("list.js", LIST_JS), ("session.js", SESSION_JS)] {
        for sink in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "createContextualFragment",
        ] {
            assert!(!js.contains(sink), "{name} reaches for {sink}");
        }
    }
}

/// The CSP refuses inline script and style; the pages carry none.
#[test]
fn pages_have_no_inline_script_style_or_handlers() {
    for (name, html) in [
        ("list.html", LIST_HTML),
        ("session.html", SESSION_HTML),
        ("the not-found page", NOT_FOUND_HTML),
    ] {
        let want = usize::from(name != "the not-found page");
        assert_eq!(
            html.matches("<script").count(),
            want,
            "{name} has exactly its own script"
        );
        for (i, _) in html.match_indices("<script") {
            let tag = &html[i..];
            let end = tag.find("</script>").expect("closed");
            assert!(
                tag.starts_with("<script src=\"") && tag[..end].ends_with("\">"),
                "{name}: a script tag with inline code near byte {i}"
            );
        }
        assert!(
            !html.contains(" style=\""),
            "{name} carries an inline style"
        );
        assert!(!html.contains("<style"), "{name} carries a style block");
        for (i, _) in html.match_indices(" on") {
            let rest = &html[i + 3..];
            let name_len = rest.chars().take_while(char::is_ascii_lowercase).count();
            assert!(
                !(name_len > 0 && rest[name_len..].starts_with('=')),
                "{name}: an inline handler near byte {i}"
            );
        }
    }
}

/// The keys the session page answers, and the motion rule.
#[test]
fn session_page_names_its_keys_and_motion_rule() {
    for key in ["\"ArrowLeft\"", "\"ArrowRight\"", "\"Escape\"", "\" \""] {
        assert!(SESSION_JS.contains(key), "session.js handles {key}");
    }
    assert!(NETS_CSS.contains("prefers-reduced-motion: reduce"));
    assert!(
        NETS_CSS.contains(".missing"),
        "the not-found page is styled"
    );
}

/// The first image file of a session, as the log names it, percent-encoded.
fn first_image(s: &Setup, net: &str, session: &str) -> Option<(String, PathBuf)> {
    let e = s.pages.archive().find(net, session).unwrap();
    let rows = read_rows(&e.dir).unwrap();
    rows.iter().find_map(|r| match &r.kind {
        LogKind::Image { file, .. } => {
            Some((crate::view::encode(file), e.dir.join("images").join(file)))
        }
        _ => None,
    })
}

/// Log a picture in `dir` and write its file, as the recorder would.
fn log_image(dir: &Path, file: &str, talker: Option<&str>) {
    std::fs::create_dir_all(dir.join("images")).unwrap();
    std::fs::write(
        dir.join("images").join(file),
        b"\x89PNG\r\n\x1a\nnot really",
    )
    .unwrap();
    crate::log::append_row(
        dir,
        &crate::model::LogRow {
            t: "2026-11-06T02:00:30Z".into(),
            kind: LogKind::Image {
                start: "2026-11-06T02:00:20Z".into(),
                offset_ms: 20_000,
                mode: "PD120".into(),
                talker: talker.map(str::to_owned),
                file: file.to_owned(),
                partial: false,
            },
        },
    )
    .unwrap();
}

#[test]
fn logged_image_is_served_as_png() {
    let s = setup("image");
    let server = serve(&s.pages);
    let addr = server.local_addr();
    let (name, path) = first_image(&s, &s.f.public_net, &s.f.public_session).unwrap();
    let url = format!(
        "/nets/{}/{}/images/{name}",
        s.f.public_net, s.f.public_session
    );
    let (h, b) = fetch(addr, "GET", &url, "");
    assert_eq!(status(&h), 200, "{h}");
    assert!(h.contains("Content-Type: image/png\r\n"), "{h}");
    assert!(h.contains("X-Content-Type-Options: nosniff\r\n"), "{h}");
    let want = std::fs::read(path).unwrap();
    assert!(want.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(b, want);
    // The same name under overs/ is not an over the log recorded.
    let (h, _) = fetch(
        addr,
        "GET",
        &format!(
            "/nets/{}/{}/overs/{name}",
            s.f.public_net, s.f.public_session
        ),
        "",
    );
    assert_eq!(status(&h), 404, "{h}");
    server.stop();
}

#[test]
fn unlogged_image_name_is_404() {
    let s = setup("imagenolog");
    let e = s
        .pages
        .archive()
        .find(&s.f.public_net, &s.f.public_session)
        .unwrap();
    // A file on disk that no row names.
    std::fs::write(e.dir.join("images").join("009 x.png"), b"png").unwrap();
    let base = format!("/nets/{}/{}", s.f.public_net, s.f.public_session);
    let missing = get(&s.pages, Access::Admin, "/nets/nope/nope/data.json");
    for path in [
        format!("{base}/images/009%20x.png"),
        format!("{base}/images/..%2Fsession.json"),
        format!("{base}/images/"),
        format!("{base}/images/%2E%2E%2Fsession.flac"),
    ] {
        assert_eq!(get(&s.pages, Access::Admin, &path), missing, "{path}");
    }
    // An over's file is not an image.
    let (over, _) = first_over(&s, &s.f.public_net, &s.f.public_session);
    assert_eq!(
        get(&s.pages, Access::Admin, &format!("{base}/images/{over}")),
        missing
    );
}

#[test]
fn private_session_image_is_404_for_anonymous() {
    let s = setup("imagepriv");
    let e = s
        .pages
        .archive()
        .find(THURSDAY, &s.f.private_session)
        .unwrap();
    log_image(&e.dir, "001 19-00-20 PD120 KX0AAA.png", Some("KX0AAA"));
    let server = serve(&s.pages);
    let addr = server.local_addr();
    let name = "001%2019-00-20%20PD120%20KX0AAA.png";
    let private = format!("/nets/{THURSDAY}/{}/images/{name}", s.f.private_session);
    let missing = format!("/nets/nope/nope/images/{name}");
    for method in ["GET", "HEAD"] {
        let (ph, pb) = fetch(addr, method, &private, "");
        let (mh, mb) = fetch(addr, method, &missing, "");
        assert_eq!(status(&ph), 404, "{method}: {ph}");
        assert_eq!((ph, pb), (mh, mb), "{method}");
    }
    // A signed-in viewer gets it.
    let (h, _) = fetch(addr, "GET", &private, "X-Test-Access: viewer\r\n");
    assert_eq!(status(&h), 200, "{h}");
    assert!(h.contains("Content-Type: image/png\r\n"), "{h}");
    server.stop();
}

#[test]
fn hostile_talker_on_an_image_is_text() {
    let s = setup("imagehostile");
    let e = s
        .pages
        .archive()
        .find(&s.f.public_net, &s.f.public_session)
        .unwrap();
    let raw = "<img src=x onerror=alert(1)>\u{202e}\"</script>";
    log_image(&e.dir, "002 19-00-20 PD120 unknown.png", Some(raw));
    let r = get(
        &s.pages,
        Access::Anonymous,
        &format!("/nets/{}/{}/data.json", s.f.public_net, s.f.public_session),
    );
    assert_eq!(r.status, 200);
    let text = String::from_utf8(r.body).unwrap();
    // It travels as a JSON string: parsing gives back exactly the base.
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let img = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "image" && i["seq"] == 3)
        .unwrap();
    let want = crate::callsign::split_call(raw).unwrap();
    assert_eq!(img["call"], serde_json::Value::String(want.base));
    assert!(want.suffix.is_some(), "the hostile text has a suffix too");
    assert_eq!(img["suffix"], serde_json::json!(want.suffix));
    // The page sets it as text: the alt is built from strings, never markup.
    assert!(SESSION_JS.contains("\"image\""));
    for sink in ["innerHTML", "outerHTML", "insertAdjacentHTML"] {
        assert!(!SESSION_JS.contains(sink));
    }
    // An alt is read aloud or shown as plain text, never laid out, so the
    // picture's carries no bidi isolates; the caption isolates with <bdi>.
    let from = SESSION_JS.find("function picture(").unwrap();
    let to = from + SESSION_JS[from..].find("function divider(").unwrap();
    let picture = &SESSION_JS[from..to];
    assert!(picture.contains("img.alt"));
    for c in ['\u{2068}', '\u{2069}'] {
        assert!(!picture.contains(c), "isolate {c:?} in the picture's alt");
    }
}

#[test]
fn private_video_refuses_anonymous() {
    let s = setup("video404");
    let e = s
        .pages
        .archive()
        .find(THURSDAY, &s.f.private_session)
        .unwrap();
    std::fs::write(e.dir.join("video.mp4"), b"not really a video").unwrap();
    let path = format!("/nets/{THURSDAY}/{}/video.mp4", s.f.private_session);
    let r = get(&s.pages, Access::Anonymous, &path);
    assert_eq!(r.status, 404);
    assert_eq!(r.body, br#"{"error":"no such session"}"#);
    let reply = handle(&s.pages, Access::Viewer, "GET", &path);
    let NetsReply::File { mime, .. } = reply else {
        panic!("{reply:?}")
    };
    assert_eq!(mime, "video/mp4");
}

#[test]
fn public_video_is_served_with_ranges() {
    let s = setup("video");
    let e = s
        .pages
        .archive()
        .find(&s.f.public_net, &s.f.public_session)
        .unwrap();
    let whole: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(e.dir.join("video.mp4"), &whole).unwrap();
    let server = serve(&s.pages);
    let addr = server.local_addr();
    let path = format!("/nets/{}/{}/video.mp4", s.f.public_net, s.f.public_session);
    let (h, b) = fetch(addr, "GET", &path, "Range: bytes=100-199\r\n");
    assert_eq!(status(&h), 206, "{h}");
    assert_eq!(b, &whole[100..200]);
    assert!(h.contains("Content-Type: video/mp4\r\n"), "{h}");
    let (h, b) = fetch(addr, "GET", &path, "");
    assert_eq!(status(&h), 200, "{h}");
    assert_eq!(b, whole);
    // A session with no video says so with the session 404.
    std::fs::remove_file(e.dir.join("video.mp4")).unwrap();
    let (h, _) = fetch(addr, "GET", &path, "");
    assert_eq!(status(&h), 404, "{h}");
    server.stop();
}

#[test]
fn data_json_names_the_video_only_when_there_is_one() {
    let s = setup("videolink");
    let e = s
        .pages
        .archive()
        .find(&s.f.public_net, &s.f.public_session)
        .unwrap();
    let data = format!("/nets/{}/{}/data.json", s.f.public_net, s.f.public_session);
    let video = |access| {
        let r = get(&s.pages, access, &data);
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        v.get("video").cloned()
    };
    assert_eq!(video(Access::Anonymous), None, "no video.mp4 yet");
    // A part file is a render still going, not a video.
    std::fs::write(e.dir.join("video.mp4.part"), b"half").unwrap();
    assert_eq!(video(Access::Anonymous), None);
    std::fs::write(e.dir.join("video.mp4"), b"not really a video").unwrap();
    let want = serde_json::json!(format!(
        "/nets/{}/{}/video.mp4",
        s.f.public_net, s.f.public_session
    ));
    assert_eq!(video(Access::Anonymous), Some(want.clone()));
    assert_eq!(video(Access::Admin), Some(want));
    // A link named video.mp4 is not served, so it is not offered either.
    std::fs::remove_file(e.dir.join("video.mp4")).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(e.dir.join("video.mp4.part"), e.dir.join("video.mp4")).unwrap();
        assert_eq!(video(Access::Anonymous), None);
    }
}

#[test]
fn private_session_names_its_video_only_to_those_who_may_see_it() {
    let s = setup("videopriv");
    let e = s
        .pages
        .archive()
        .find(THURSDAY, &s.f.private_session)
        .unwrap();
    std::fs::write(e.dir.join("video.mp4"), b"not really a video").unwrap();
    let data = format!("/nets/{THURSDAY}/{}/data.json", s.f.private_session);
    assert_eq!(get(&s.pages, Access::Anonymous, &data).status, 404);
    let r = get(&s.pages, Access::Viewer, &data);
    assert_eq!(r.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(
        v["video"],
        serde_json::json!(format!(
            "/nets/{THURSDAY}/{}/video.mp4",
            s.f.private_session
        ))
    );
}

#[test]
fn the_page_offers_the_video_as_a_link() {
    // The header's Video button is an anchor to the URL data.json gives,
    // hidden until there is one, set as an attribute and never as markup.
    assert!(SESSION_HTML.contains(r#"id="video-btn""#));
    let at = SESSION_HTML.find(r#"id="video-btn""#).unwrap();
    let tag_start = SESSION_HTML[..at].rfind('<').unwrap();
    let tag = &SESSION_HTML[tag_start..at + SESSION_HTML[at..].find('>').unwrap()];
    assert!(tag.starts_with("<a "), "{tag}");
    assert!(tag.contains(" hidden"), "{tag}");
    assert!(SESSION_JS.contains("d.video"));
}
