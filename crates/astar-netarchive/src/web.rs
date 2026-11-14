// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `/nets/` routes: the recordings pages, their JSON, and the audio.
//!
//! Whoever mounts these decides who is asking ([`Access`]) and passes it in
//! with every request. Every route that names a session looks it up with
//! [`Archive::find_for`], which applies [`Access::may_see`] on that same
//! request, so a session the asker may not see answers exactly as a missing
//! one does. Nothing is cached across access levels: the only caches are
//! the archive's peaks and check-in counts, which are the same for everyone.
//!
//! A URL never becomes a path. Sessions are matched against the folders the
//! scan found, and an over file is served only when its decoded name is one
//! the session's own log recorded.

use std::path::{Path, PathBuf};

use astar_http::files::send_file;
use astar_http::{Conn, Reply, Response, StreamClass};
use astar_record::tarball::open_regular;
use serde::Deserialize;

use crate::index::{Access, Archive, SessionEntry, decode_segment};
use crate::model::LogKind;
use crate::retain::{DeleteRefusal, delete_session};
use crate::view::{ViewOpts, encode, list_view, session_url, session_view};

/// The Content-Security-Policy on every `/nets/` answer.
pub const NETS_CSP: &str = "default-src 'self'; style-src 'self'; script-src 'self'; media-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The pages' stylesheet. Public so astar-server can check that the tokens
/// it shares with its own stylesheet still match.
pub const NETS_CSS: &str = include_str!("assets/nets.css");
const LIST_HTML: &str = include_str!("assets/list.html");
const LIST_JS: &str = include_str!("assets/list.js");
const SESSION_HTML: &str = include_str!("assets/session.html");
const SESSION_JS: &str = include_str!("assets/session.js");

/// The answer to a session that is missing or hidden, for JSON and files.
const NO_SUCH_SESSION: &str = r#"{"error":"no such session"}"#;

/// The answer to a page that is missing or hidden. It does not say which.
const NOT_FOUND_HTML: &str = "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>No such recording</title>\n<link rel=\"stylesheet\" href=\"/nets/nets.css\">\n</head>\n<body>\n<main class=\"missing\">\n<h1>No such recording</h1>\n<p>It may have been deleted, or you may need to sign in to see it.</p>\n<p><a href=\"/nets/\">All recordings</a></p>\n</main>\n</body>\n</html>\n";

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

/// The recordings pages over one archive.
#[derive(Debug)]
pub struct Pages {
    archive: Archive,
    opts: ViewOpts,
}

/// One request, as the mounting server hands it over.
#[derive(Debug, Clone, Copy)]
pub struct NetsRequest<'a> {
    pub method: &'a str,
    /// The path, still percent-encoded, without the query.
    pub path: &'a str,
    /// Who is asking, decided by the mounting server for this request.
    pub access: Access,
    /// Whether this request's bind serves the pages' sign-in URL. A server
    /// with sign-in on one bind only passes `false` on the others, so a
    /// signed-out reader there is never sent to a page that is not there.
    pub offer_sign_in: bool,
}

/// What a `/nets/` route answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetsReply {
    /// A whole answer.
    Response(Response),
    /// A file of the archive, streamed with byte ranges by
    /// [`NetsReply::into_reply`].
    File { path: PathBuf, mime: &'static str },
}

impl NetsReply {
    /// The reply for the server. `range` is the request's `Range` header.
    /// A `File` becomes a per-client stream that opens the file again
    /// without following links (the parent folder must be a real folder,
    /// the file a regular one) and sends it with `Cache-Control: no-store`.
    /// If that open fails, it answers the same 404 as a missing session.
    #[must_use]
    pub fn into_reply(self, range: Option<String>) -> Reply {
        match self {
            Self::Response(r) => Reply::Response(r),
            Self::File { path, mime } => Reply::Stream {
                class: StreamClass::PerClientOnly,
                run: Box::new(move |conn| stream_file(conn, &path, mime, range.as_deref())),
            },
        }
    }
}

fn stream_file(conn: Conn, path: &Path, mime: &str, range: Option<&str>) {
    let parent_is_real = path
        .parent()
        .and_then(|p| std::fs::symlink_metadata(p).ok())
        .is_some_and(|m| m.file_type().is_dir());
    let opened = if parent_is_real {
        open_regular(path).unwrap_or_else(|e| {
            tracing::warn!("cannot open {}: {e}", path.display());
            None
        })
    } else {
        None
    };
    let Some((file, size)) = opened else {
        conn.respond(&no_such_session());
        return;
    };
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    send_file(
        conn,
        file,
        size,
        &[
            ("Content-Type", mime),
            ("Cache-Control", "no-store"),
            ("Content-Security-Policy", NETS_CSP),
            ("X-Content-Type-Options", "nosniff"),
        ],
        range,
        &name,
    );
}

/// The headers every answer carries.
fn secured(r: Response) -> Response {
    r.header("Content-Security-Policy", NETS_CSP)
        .header("X-Content-Type-Options", "nosniff")
}

fn json(status: u16, body: impl Into<Vec<u8>>) -> Response {
    secured(Response::json(status, body).header("Cache-Control", "no-store"))
}

fn no_such_session() -> Response {
    json(404, NO_SUCH_SESSION)
}

fn not_found_page() -> Response {
    secured(
        Response::new(404)
            .with_body(HTML, NOT_FOUND_HTML)
            .header("Cache-Control", "no-store"),
    )
}

fn asset(content_type: &str, body: &str) -> Response {
    secured(
        Response::new(200)
            .with_body(content_type, body)
            .header("Cache-Control", "no-cache"),
    )
}

fn redirect(to: &str) -> Response {
    secured(
        Response::new(308)
            .header("Location", to)
            .header("Cache-Control", "no-store"),
    )
}

fn as_json(v: &impl serde::Serialize) -> Response {
    match serde_json::to_vec(v) {
        Ok(body) => json(200, body),
        Err(e) => {
            tracing::warn!("cannot serialise a /nets/ view: {e}");
            json(500, r#"{"error":"internal error"}"#)
        }
    }
}

/// The MIME type of an archive audio file, by its extension.
fn audio_mime(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("flac") => "audio/flac",
        Some("m4a" | "mp4") => "audio/mp4",
        Some("opus" | "ogg") => "audio/ogg",
        _ => "application/octet-stream",
    }
}

/// `path` as a `File` reply when it is a regular file (not a link), else
/// the 404. [`NetsReply::into_reply`] checks again when it opens it.
fn file_reply(path: PathBuf) -> NetsReply {
    let regular = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file());
    if !regular {
        return NetsReply::Response(no_such_session());
    }
    let mime = audio_mime(&path.file_name().unwrap_or_default().to_string_lossy());
    NetsReply::File { path, mime }
}

#[derive(Deserialize)]
struct DeleteBody {
    net: String,
    session: String,
}

impl Pages {
    #[must_use]
    pub fn new(root: PathBuf, opts: ViewOpts) -> Self {
        Self {
            archive: Archive::new(root),
            opts,
        }
    }

    /// The archive these pages read.
    #[must_use]
    pub const fn archive(&self) -> &Archive {
        &self.archive
    }

    /// `/nets` or anything under `/nets/`.
    #[must_use]
    pub fn is_nets_path(path: &str) -> bool {
        path == "/nets" || path.starts_with("/nets/")
    }

    /// Answer one `GET` or `HEAD` under `/nets/`; any other method is 405.
    #[must_use]
    pub fn handle(&self, req: &NetsRequest<'_>) -> NetsReply {
        NetsReply::Response(match self.route(req) {
            Ok(reply) => return reply,
            Err(r) => r,
        })
    }

    fn route(&self, req: &NetsRequest<'_>) -> Result<NetsReply, Response> {
        if !matches!(req.method, "GET" | "HEAD") {
            return Err(secured(
                Response::json(405, r#"{"error":"method not allowed"}"#)
                    .header("Allow", "GET, HEAD")
                    .header("Cache-Control", "no-store"),
            ));
        }
        if req.path == "/nets" {
            return Err(redirect("/nets/"));
        }
        let Some(rest) = req.path.strip_prefix("/nets/") else {
            return Err(not_found_page());
        };
        let segs: Vec<&str> = rest.split('/').collect();
        let access = req.access;
        let no_sign_in;
        let opts = if req.offer_sign_in {
            &self.opts
        } else {
            no_sign_in = ViewOpts {
                sign_in_url: None,
                ..self.opts.clone()
            };
            &no_sign_in
        };
        let find = |n: &str, s: &str| self.archive.find_for(access, n, s);
        let ok = |r: Response| Ok(NetsReply::Response(r));
        match segs.as_slice() {
            [""] => ok(asset(HTML, LIST_HTML)),
            ["nets.css"] => ok(asset(CSS, NETS_CSS)),
            ["list.js"] => ok(asset(JS, LIST_JS)),
            ["session.js"] => ok(asset(JS, SESSION_JS)),
            ["index.json"] => ok(as_json(&list_view(&self.archive, access, opts))),
            [n, s] => {
                let e = find(n, s).ok_or_else(not_found_page)?;
                ok(redirect(&session_url(&e)))
            }
            [n, s, ""] => {
                find(n, s).ok_or_else(not_found_page)?;
                ok(secured(
                    Response::new(200)
                        .with_body(HTML, SESSION_HTML)
                        .header("Cache-Control", "no-store"),
                ))
            }
            [n, s, "data.json"] => {
                let e = find(n, s).ok_or_else(no_such_session)?;
                ok(as_json(&session_view(&self.archive, &e, access, opts)))
            }
            [n, s, name @ ("session.flac" | "session.m4a")] => {
                let e = find(n, s).ok_or_else(no_such_session)?;
                Ok(file_reply(e.dir.join(name)))
            }
            [n, s, "overs", file] => {
                let e = find(n, s).ok_or_else(no_such_session)?;
                let name = self.logged_over(&e, file).ok_or_else(no_such_session)?;
                Ok(file_reply(e.dir.join("overs").join(name)))
            }
            _ => Err(not_found_page()),
        }
    }

    /// The over file `segment` (percent-encoded) names, when the session's
    /// log recorded an over with exactly that file name.
    fn logged_over(&self, e: &SessionEntry, segment: &str) -> Option<String> {
        let name = decode_segment(segment)?;
        self.archive
            .rows(e)
            .iter()
            .any(|r| matches!(&r.kind, LogKind::Over { file, .. } if *file == name))
            .then_some(name)
    }

    /// Delete a finished session. `body` is
    /// `{"net":"<slug>","session":"<slug>"}`. 403 unless `access` is Admin
    /// (checked first, so a non-admin learns nothing about the session),
    /// 400 for a bad body, 404 for a session that is missing or not
    /// visible, 409 for one still recording, 200 `{"deleted":true}`.
    /// Origin and CSRF checks are the mounting server's.
    #[must_use]
    pub fn delete(&self, body: &[u8], access: Access) -> Response {
        if access != Access::Admin {
            return json(403, r#"{"error":"only an admin may delete a recording"}"#);
        }
        let Ok(b) = serde_json::from_slice::<DeleteBody>(body) else {
            return json(
                400,
                r#"{"error":"expected a JSON body with net and session"}"#,
            );
        };
        // The slugs travel as plain JSON strings; encoding them makes the
        // decoder see exactly these characters.
        let Some(e) = self
            .archive
            .find_for(access, &encode(&b.net), &encode(&b.session))
        else {
            return no_such_session();
        };
        match delete_session(self.archive.root(), &e.dir) {
            Ok(()) => json(200, r#"{"deleted":true}"#),
            Err(DeleteRefusal::Open) => json(409, r#"{"error":"that session is still open"}"#),
            Err(DeleteRefusal::NotASession) => no_such_session(),
            Err(DeleteRefusal::Failed) => {
                json(500, r#"{"error":"the session could not be fully deleted"}"#)
            }
        }
    }
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
