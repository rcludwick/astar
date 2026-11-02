// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The admin Recordings page's routes: the logger's state, deleting one
//! recording, and the two downloads (one FLAC, or every recording and
//! capture as a tar).
//!
//! Every route is admin-class (`crate::access::ADMIN_ROUTES`), so a viewer
//! or an anonymous caller never reaches this module. A file is always named
//! by its index row's `name`, never by a path: the path served or deleted
//! is the row's own.
//!
//! The router buffers a whole answer, so the downloads are not answered
//! there: both dispatchers (the control port's and the public bind's
//! `/admin/` group) ask [`download_for`] after the gate admits the request
//! and answer [`stream_reply`]. A file goes out with `Content-Length`, the
//! tar chunked; either one cut short by an error is closed without its
//! last byte or last chunk, so a client cannot take it for a whole answer.
//!
//! The file route honours one byte range (`Range: bytes=a-b`, `bytes=a-`,
//! `bytes=-n`) with a `206` and `Content-Range`, so a browser's `<audio>`
//! can seek, and Safari, which asks for a range before it will play, plays
//! at all. A range past the end is `416` with `Content-Range: bytes */N`.
//! A header asking for more than one range, or one that does not parse, is
//! ignored and the whole file is sent `200`, as RFC 9110 allows; one range
//! is all a media element ever asks for.
//!
//! A download is admitted once, at its start, and is not re-checked
//! against the session while it streams (the event stream is). It is
//! finite and admin-only, so a session revoked mid-download lets that one
//! download finish; the next request is refused.

use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use astar_http::{Chunked, Reply, Response, StreamClass};
use astar_record::clock::parse_rfc3339;
use astar_record::tarball::{DownloadPlan, TarSummary, open_regular, write_tar_planned};

use crate::controller::NodeController;
use crate::http::HttpResponse;

/// The tar's file name when it is saved.
pub const TAR_NAME: &str = "astar-recordings.tar";
/// How much of the tar is gathered into one chunk.
const TAR_CHUNK: usize = 64 * 1024;

/// A download the gate has admitted, to be streamed by [`stream_reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Download {
    /// The row's file, its `name`, and whether to send it as an attachment.
    File(PathBuf, String, bool),
    /// Every recording and capture at or after the time (all for `None`).
    Tar(Option<SystemTime>),
}

fn off() -> HttpResponse {
    HttpResponse::error_json(404, "recording is off")
}

fn no_such_recording() -> HttpResponse {
    HttpResponse::error_json(404, "no such recording")
}

fn no_store(r: HttpResponse) -> HttpResponse {
    r.with_header("Cache-Control", "no-store")
}

/// `GET /recordings/state`: `{"status": …, "rows": […]}`, rows newest first.
#[must_use]
pub fn state(ctrl: &NodeController) -> HttpResponse {
    let Some(logger) = ctrl.recordings() else {
        return off();
    };
    no_store(HttpResponse::json(
        200,
        &serde_json::json!({ "status": logger.status(), "rows": logger.rows() }),
    ))
}

#[derive(serde::Deserialize)]
struct DeleteRequest {
    name: String,
}

/// `POST /recordings/delete` with `{"name": "<name>"}`: the row and its
/// file, or 404 when no row has that name. The caller has already checked
/// that this is a same-origin JSON request.
#[must_use]
pub fn delete(ctrl: &NodeController, body: &[u8]) -> HttpResponse {
    let Some(logger) = ctrl.recordings() else {
        return off();
    };
    let req: DeleteRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
    };
    if logger.delete(&req.name) {
        HttpResponse::json(200, &serde_json::json!({ "deleted": true }))
    } else {
        no_such_recording()
    }
}

/// The download a request asks for: `None` when `(method, admin_path)` is
/// not a download route, `Some(Err(…))` for the answer that refuses it
/// (recording off, an unknown name, a bad `since`).
#[must_use]
pub fn download_for(
    ctrl: &NodeController,
    method: &str,
    admin_path: &str,
    query: &str,
) -> Option<Result<Download, HttpResponse>> {
    let tar = match (method, admin_path) {
        ("GET", "/recordings/file") => false,
        ("GET", "/recordings.tar") => true,
        _ => return None,
    };
    let Some(logger) = ctrl.recordings() else {
        return Some(Err(off()));
    };
    if tar {
        let since = match query_value(query, "since") {
            None => None,
            Some(s) => match parse_rfc3339(&s) {
                Some(t) => Some(t),
                None => {
                    return Some(Err(HttpResponse::error_json(
                        400,
                        "since must be an RFC 3339 time",
                    )));
                }
            },
        };
        return Some(Ok(Download::Tar(since)));
    }
    let name = query_value(query, "name").unwrap_or_default();
    let attach = query_value(query, "download").is_some_and(|v| v == "1");
    Some(
        logger
            .file_for(&name)
            .map(|path| Download::File(path, name, attach))
            .ok_or_else(no_such_recording),
    )
}

/// The value of `key` in a query string, percent-decoded. A value that
/// does not decode to UTF-8 is absent.
fn query_value(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('=').or(Some((kv, ""))))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| percent_decode(v))
}

/// `%XX` is exactly two ASCII hex digits; anything else (`%+1`, `%1`, a
/// trailing `%`) makes the whole value absent.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let (hi, lo) = (*b.get(i + 1)?, *b.get(i + 2)?);
            if !hi.is_ascii_hexdigit() || !lo.is_ascii_hexdigit() {
                return None;
            }
            let hex = [hi, lo];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// One `Range` header against a file of `total` bytes, as the inclusive
/// span to send. `None`: ignore the header and send the whole file (not a
/// `bytes` range, malformed, `a > b`, or more than one range).
/// `Some(Err(()))`: unsatisfiable, answered `416`.
fn parse_range(header: &str, total: u64) -> Option<Result<(u64, u64), ()>> {
    // The unit is case-insensitive (RFC 9110 §14.1).
    let h = header.trim();
    let spec = h
        .get(..5)
        .filter(|u| u.eq_ignore_ascii_case("bytes"))
        .map(|_| &h[5..])?
        .trim_start()
        .strip_prefix('=')?
        .trim();
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    // A number too long for u64 is still a number: it saturates.
    let num = |s: &str| s.parse::<u64>().unwrap_or(u64::MAX);
    if a.is_empty() {
        if !digits(b) {
            return None;
        }
        let n = num(b);
        if n == 0 || total == 0 {
            return Some(Err(()));
        }
        return Some(Ok((total.saturating_sub(n), total - 1)));
    }
    if !digits(a) || !(b.is_empty() || digits(b)) {
        return None;
    }
    let first = num(a);
    let last = if b.is_empty() { u64::MAX } else { num(b) };
    if last < first {
        return None;
    }
    if first >= total {
        return Some(Err(()));
    }
    Some(Ok((first, last.min(total - 1))))
}

/// A download as the server sends it, inside a per-client stream slot
/// (like the audio streams, it does not count against the event streams).
/// `range` is the request's `Range` header, read only for a file.
#[must_use]
pub fn stream_reply(ctrl: &Arc<NodeController>, d: Download, range: Option<&str>) -> Reply {
    let ctrl = Arc::clone(ctrl);
    let range = range.map(str::to_owned);
    Reply::Stream {
        class: StreamClass::PerClientOnly,
        run: Box::new(move |conn| match d {
            Download::File(path, name, attach) => {
                run_file(conn, &path, &name, attach, range.as_deref());
            }
            Download::Tar(since) => run_tar(conn, &ctrl, since),
        }),
    }
}

/// A refusal decided inside the slot, as a whole answer.
fn respond(conn: astar_http::Conn, r: &HttpResponse) {
    conn.respond(
        &Response::new(r.status)
            .with_body(r.content_type, r.body.clone())
            .header("Cache-Control", "no-store"),
    );
}

fn run_file(
    mut conn: astar_http::Conn,
    path: &Path,
    name: &str,
    attach: bool,
    range: Option<&str>,
) {
    // Opened only now, and never through a symlink: the file or its day
    // folder may have changed since the name was looked up.
    let day_ok = path
        .parent()
        .is_some_and(|d| std::fs::symlink_metadata(d).is_ok_and(|m| m.file_type().is_dir()));
    let opened = if day_ok {
        open_regular(path).ok().flatten()
    } else {
        None
    };
    let Some((mut file, size)) = opened else {
        respond(conn, &no_such_recording());
        return;
    };
    let disposition = format!("attachment; filename=\"{name}\"");
    let mut headers = vec![
        ("Content-Type", "audio/flac"),
        ("Cache-Control", "no-store"),
        ("Accept-Ranges", "bytes"),
    ];
    if attach {
        headers.push(("Content-Disposition", disposition.as_str()));
    }
    // The span comes from the size of the open handle, and the seek is on
    // that same handle: nothing is re-opened by path.
    let (status, start, len, content_range) = match range.and_then(|h| parse_range(h, size)) {
        None => (200, 0, size, None),
        Some(Ok((first, last))) => (
            206,
            first,
            last - first + 1,
            Some(format!("bytes {first}-{last}/{size}")),
        ),
        Some(Err(())) => {
            conn.respond(
                &Response::json(416, r#"{"error":"range not satisfiable"}"#)
                    .header("Content-Range", format!("bytes */{size}"))
                    .header("Accept-Ranges", "bytes")
                    .header("Cache-Control", "no-store"),
            );
            return;
        }
    };
    if let Some(cr) = &content_range {
        headers.push(("Content-Range", cr.as_str()));
    }
    if start > 0
        && let Err(e) = file.seek(SeekFrom::Start(start))
    {
        tracing::warn!("recording: cannot seek {name}: {e}");
        respond(
            conn,
            &HttpResponse::error_json(500, "cannot read the recording"),
        );
        return;
    }
    if conn.write_head_sized(status, &headers, len).is_err() || conn.head_only() {
        return;
    }
    if let Err(e) = copy_exact(file, &mut conn, len) {
        // Returning closes the connection short of Content-Length.
        tracing::warn!("recording: {name} cut short: {e}");
    }
}

/// Copies exactly `size` bytes; a source that runs dry first is an error.
fn copy_exact<R: Read, W: Write>(mut from: R, to: &mut W, size: u64) -> io::Result<()> {
    let mut left = size;
    let mut buf = vec![0u8; TAR_CHUNK];
    while left > 0 {
        let cap = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
        let n = from.read(&mut buf[..cap])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the file shrank while it was being sent",
            ));
        }
        to.write_all(&buf[..n])?;
        left -= n as u64;
    }
    to.flush()
}

fn run_tar(mut conn: astar_http::Conn, ctrl: &NodeController, since: Option<SystemTime>) {
    let Some(logger) = ctrl.recordings() else {
        respond(conn, &off());
        return;
    };
    let plan = logger.plan_download(since);
    let disposition = format!("attachment; filename=\"{TAR_NAME}\"");
    let headers = [
        ("Content-Type", "application/x-tar"),
        ("Content-Disposition", disposition.as_str()),
        ("Cache-Control", "no-store"),
    ];
    if conn.write_head_chunked(200, &headers).is_err() || conn.head_only() {
        return;
    }
    match send_tar(conn, &plan) {
        Ok(s) if s.skipped > 0 => {
            tracing::info!(
                "recording: download sent {} file(s), skipped {} gone since it began",
                s.sent,
                s.skipped
            );
        }
        Ok(_) => {}
        // Returning without the last chunk tells the client it is cut.
        Err(e) => tracing::warn!("recording: download cut short: {e}"),
    }
}

/// `plan` as a chunked tar onto `out`, ending with the last chunk only when
/// the whole tar was written.
fn send_tar<W: Write>(out: W, plan: &DownloadPlan) -> io::Result<TarSummary> {
    let mut w = BufWriter::with_capacity(TAR_CHUNK, Chunked::new(out));
    let summary = write_tar_planned(&mut w, plan)?;
    w.into_inner()
        .map_err(io::IntoInnerError::into_error)?
        .finish()?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ByteSize, RecordingCfg};
    use crate::recording::index::{self, IndexRow};
    use crate::recording::logger::{NodeLogger, RecordSource};
    use crate::secrets::SecretProvider;
    use std::collections::BTreeMap;
    use std::net::TcpStream;
    use std::time::Duration;

    struct Rig {
        ctrl: Arc<NodeController>,
        logger: Arc<NodeLogger>,
        dir: tempfile::TempDir,
        older: IndexRow,
        newer: IndexRow,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.logger.stop();
        }
    }

    fn controller() -> Arc<NodeController> {
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        Arc::new(NodeController::new(station, SecretProvider::new()))
    }

    /// A row as the logger would have left it, with `bytes` of file.
    fn seed(dir: &Path, ago: Duration, tag: &str, bytes: usize) -> IndexRow {
        let started_at = astar_record::clock::rfc3339_utc(SystemTime::now() - ago);
        let name = format!("seed-{tag}.flac");
        let path = format!("{}/{name}", &started_at[..10]);
        std::fs::create_dir_all(dir.join(&started_at[..10])).unwrap();
        std::fs::write(dir.join(&path), vec![b'f'; bytes]).unwrap();
        let row = IndexRow {
            name,
            path,
            started_at,
            duration_ms: 1000,
            talkers: vec!["W1ABC".into()],
            network: "allstar".into(),
            bytes: bytes as u64,
        };
        index::append(dir, &row).unwrap();
        row
    }

    #[allow(clippy::unnecessary_wraps)] // the free-space seam's signature
    fn plenty(_: &Path) -> Option<u64> {
        Some(10_000_000_000)
    }

    /// A logger over a temp folder with two rows, two days and an hour old,
    /// installed on a controller with nothing on the air.
    fn rig_with(before_start: impl FnOnce(&Path)) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let older = seed(dir.path(), Duration::from_secs(2 * 86_400), "older", 1000);
        let newer = seed(dir.path(), Duration::from_secs(3600), "newer", 2000);
        before_start(dir.path());
        let cfg = RecordingCfg {
            enabled: true,
            dir: dir.path().to_path_buf(),
            max_bytes: ByteSize {
                bytes: 2_000_000_000,
                text: "2 GB".into(),
            },
            max_age_days: 30,
            frames: false,
        };
        let ctrl = controller();
        let logger =
            NodeLogger::start_with(&cfg, Arc::clone(&ctrl) as Arc<dyn RecordSource>, plenty);
        ctrl.set_recordings(Arc::clone(&logger));
        Rig {
            ctrl,
            logger,
            dir,
            older,
            newer,
        }
    }

    fn rig() -> Rig {
        rig_with(|_| {})
    }

    fn body_json(r: &HttpResponse) -> serde_json::Value {
        serde_json::from_slice(&r.body).unwrap()
    }

    /// One request to a fresh control port; the head and the raw body.
    fn fetch(ctrl: &Arc<NodeController>, target: &str) -> (String, Vec<u8>) {
        fetch_with(ctrl, target, "")
    }

    /// [`fetch`] with extra header lines (each ending `\r\n`).
    fn fetch_with(ctrl: &Arc<NodeController>, target: &str, extra: &str) -> (String, Vec<u8>) {
        let server = crate::server::bind_control(ctrl, "127.0.0.1:0").unwrap();
        let mut s = TcpStream::connect(server.local_addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            s,
            "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut all = Vec::new();
        let _ = s.read_to_end(&mut all);
        server.stop();
        split(&all)
    }

    fn split(all: &[u8]) -> (String, Vec<u8>) {
        let i = all
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a head");
        (
            String::from_utf8_lossy(&all[..i + 4]).into_owned(),
            all[i + 4..].to_vec(),
        )
    }

    /// A chunked body: its bytes, and whether the last chunk arrived.
    fn dechunk(mut rest: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        loop {
            let Some(nl) = rest.windows(2).position(|w| w == b"\r\n") else {
                return (out, false);
            };
            let Ok(size) = usize::from_str_radix(std::str::from_utf8(&rest[..nl]).unwrap(), 16)
            else {
                return (out, false);
            };
            rest = &rest[nl + 2..];
            if size == 0 {
                return (out, rest == b"\r\n");
            }
            if rest.len() < size + 2 {
                out.extend_from_slice(&rest[..size.min(rest.len())]);
                return (out, false);
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
    }

    fn untar(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut a = tar::Archive::new(bytes);
        let mut out = BTreeMap::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            let p = e.path().unwrap().to_string_lossy().into_owned();
            let mut body = Vec::new();
            e.read_to_end(&mut body).unwrap();
            out.insert(p, body);
        }
        out
    }

    /// The whole tarball, which must have arrived complete.
    fn tarball(ctrl: &Arc<NodeController>, target: &str) -> BTreeMap<String, Vec<u8>> {
        let (head, body) = fetch(ctrl, target);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            head.contains("Content-Type: application/x-tar\r\n"),
            "{head}"
        );
        assert!(
            head.contains("Content-Disposition: attachment; filename=\"astar-recordings.tar\"\r\n"),
            "{head}"
        );
        assert!(head.contains("Transfer-Encoding: chunked\r\n"), "{head}");
        let (tar, complete) = dechunk(&body);
        assert!(complete, "the last chunk arrived");
        untar(&tar)
    }

    fn query(name: &str) -> String {
        format!("name={name}")
    }

    #[test]
    fn state_lists_rows_newest_first() {
        let r = rig();
        let resp = state(&r.ctrl);
        assert_eq!(resp.status, 200);
        let v = body_json(&resp);
        let names: Vec<&str> = v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["seed-newer.flac", "seed-older.flac"]);
        assert_eq!(v["rows"][0]["talkers"][0], "W1ABC");
        assert!(v["status"]["state"].is_string(), "{v}");
        assert!(v["status"]["total_bytes"].as_u64().unwrap() >= 3000, "{v}");
        // And off: no logger, a 404 that says so.
        let off = state(&controller());
        assert_eq!(
            (off.status, body_json(&off)["error"].clone()),
            (404, "recording is off".into())
        );
    }

    #[test]
    fn file_and_delete_accept_only_indexed_names() {
        let r = rig();
        let outside = r.dir.path().join("outside.flac");
        std::fs::write(&outside, b"not a recording").unwrap();
        for bad in [
            "../../etc/passwd",
            "%2e%2e/x",
            "%2e%2e%2f%2e%2e%2fetc%2fpasswd",
            "",
            "outside.flac",
            r.newer.path.as_str(),
            "seed-newer.flac%00",
            "%zz",
        ] {
            let got = download_for(&r.ctrl, "GET", "/recordings/file", &query(bad));
            let Some(Err(resp)) = got else {
                panic!("{bad:?}: {got:?}");
            };
            assert_eq!(resp.status, 404, "{bad:?}");
            let body = serde_json::json!({ "name": percent_decode(bad).unwrap_or_default() });
            let resp = delete(&r.ctrl, body.to_string().as_bytes());
            assert_eq!(resp.status, 404, "{bad:?}");
        }
        assert!(
            matches!(
                download_for(&r.ctrl, "GET", "/recordings/file", ""),
                Some(Err(HttpResponse { status: 404, .. }))
            ),
            "no name at all"
        );
        assert_eq!(r.logger.rows().len(), 2, "nothing deleted");
        assert!(r.dir.path().join(&r.newer.path).exists());
        assert!(r.dir.path().join(&r.older.path).exists());
        assert_eq!(std::fs::read(&outside).unwrap(), b"not a recording");
        // The indexed name resolves to the row's own path.
        let got = download_for(
            &r.ctrl,
            "GET",
            "/recordings/file",
            &query("seed-newer.flac"),
        );
        assert_eq!(
            got,
            Some(Ok(Download::File(
                r.dir.path().join(&r.newer.path),
                "seed-newer.flac".into(),
                false
            )))
        );
        let got = download_for(
            &r.ctrl,
            "GET",
            "/recordings/file",
            "name=seed-newer.flac&download=1",
        );
        assert!(
            matches!(got, Some(Ok(Download::File(_, _, true)))),
            "{got:?}"
        );
        // Not a download route at all.
        assert_eq!(download_for(&r.ctrl, "GET", "/recordings/state", ""), None);
        assert_eq!(download_for(&r.ctrl, "POST", "/recordings.tar", ""), None);
    }

    #[test]
    fn the_file_streams_with_its_length_and_attaches_only_when_asked() {
        let r = rig();
        let (head, body) = fetch(&r.ctrl, "/recordings/file?name=seed-newer.flac");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Content-Type: audio/flac\r\n"), "{head}");
        assert!(head.contains("Content-Length: 2000\r\n"), "{head}");
        assert!(!head.contains("Content-Disposition"), "{head}");
        assert_eq!(body, vec![b'f'; 2000]);
        let (head, _) = fetch(
            &r.ctrl,
            "/admin/recordings/file?name=seed-newer.flac&download=1",
        );
        assert!(
            head.contains("Content-Disposition: attachment; filename=\"seed-newer.flac\"\r\n"),
            "{head}"
        );
        let (head, body) = fetch(&r.ctrl, "/recordings/file?name=..%2F..%2Fetc%2Fpasswd");
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains("no such recording"));
    }

    #[test]
    fn a_file_that_vanishes_after_the_lookup_is_a_404_not_a_follow() {
        let r = rig();
        let Some(Ok(d)) = download_for(
            &r.ctrl,
            "GET",
            "/recordings/file",
            &query("seed-newer.flac"),
        ) else {
            panic!()
        };
        let path = r.dir.path().join(&r.newer.path);
        std::fs::remove_file(&path).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(r.dir.path().join(&r.older.path), &path).unwrap();
        let server = astar_http::Server::bind("127.0.0.1:0", astar_http::Limits::loopback(), {
            let ctrl = Arc::clone(&r.ctrl);
            move |_| stream_reply(&ctrl, d.clone(), None)
        })
        .unwrap();
        let mut s = TcpStream::connect(server.local_addr()).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut all = Vec::new();
        let _ = s.read_to_end(&mut all);
        server.stop();
        let (head, _) = split(&all);
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    }

    #[test]
    fn delete_removes_the_file_and_the_row() {
        let r = rig();
        let resp = delete(&r.ctrl, br#"{"name":"seed-older.flac"}"#);
        assert_eq!(resp.status, 200);
        assert_eq!(body_json(&resp), serde_json::json!({ "deleted": true }));
        assert!(!r.dir.path().join(&r.older.path).exists());
        let names: Vec<String> = r.logger.rows().into_iter().map(|row| row.name).collect();
        assert_eq!(names, vec!["seed-newer.flac"]);
        assert_eq!(
            delete(&r.ctrl, br#"{"name":"seed-older.flac"}"#).status,
            404
        );
        assert_eq!(delete(&r.ctrl, b"not json").status, 400);
        assert_eq!(delete(&controller(), br#"{"name":"x"}"#).status, 404);
    }

    #[test]
    fn tar_since_filters_and_rejects_garbage() {
        let r = rig();
        let all = tarball(&r.ctrl, "/recordings.tar");
        assert!(all.contains_key(&format!("recordings/{}", r.older.path)));
        assert!(all.contains_key(&format!("recordings/{}", r.newer.path)));
        let between =
            astar_record::clock::rfc3339_utc(SystemTime::now() - Duration::from_secs(86_400));
        let since = percent_encode(&between);
        let files = tarball(&r.ctrl, &format!("/recordings.tar?since={since}"));
        let recordings: Vec<&String> = files
            .keys()
            .filter(|k| Path::new(k).extension().is_some_and(|e| e == "flac"))
            .collect();
        assert_eq!(recordings, vec![&format!("recordings/{}", r.newer.path)]);
        assert_eq!(
            files[&format!("recordings/{}", r.newer.path)],
            vec![b'f'; 2000]
        );
        let index = String::from_utf8(files["recordings/index.jsonl"].clone()).unwrap();
        assert_eq!(index.lines().count(), 1, "{index}");
        assert!(index.contains("seed-newer.flac"));
        for bad in ["yesterday", "2026-10-27", ""] {
            let got = download_for(&r.ctrl, "GET", "/recordings.tar", &format!("since={bad}"));
            let Some(Err(resp)) = got else {
                panic!("{bad:?}: {got:?}")
            };
            assert_eq!(resp.status, 400, "{bad:?}");
            assert_eq!(body_json(&resp)["error"], "since must be an RFC 3339 time");
        }
        let (head, _) = fetch(&r.ctrl, "/recordings.tar?since=yesterday");
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        let (head, _) = fetch(&controller(), "/recordings.tar");
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    }

    fn percent_encode(s: &str) -> String {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b == b'-' {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    }

    #[test]
    fn the_tarball_unpacks_as_an_unamblify_capture_set() {
        use astar_record::frame_writer::FrameWriter;
        use astar_record::frames::{CaptureMode, Dir, FrameEvent};
        let r = rig_with(|dir| {
            let mut w = FrameWriter::new(dir, "0.1.16-beta");
            let t = std::time::Instant::now();
            for (dir, talker) in [(Dir::Rx, "W1ABC"), (Dir::Tx, "K2XYZ")] {
                w.handle(
                    FrameEvent::Start {
                        dir,
                        network: "m17",
                        mode: CaptureMode::Codec2_3200,
                        talker: Some(talker.into()),
                        at: SystemTime::now(),
                        vocoder: "codec2",
                    },
                    t,
                )
                .unwrap();
                for _ in 0..5 {
                    w.handle(
                        FrameEvent::Frame {
                            dir,
                            bytes: vec![0x55; 8],
                            seq: None,
                        },
                        t,
                    )
                    .unwrap();
                    w.handle(
                        FrameEvent::Pcm {
                            dir,
                            pcm: [100; 160],
                        },
                        t,
                    )
                    .unwrap();
                }
                w.handle(FrameEvent::End { dir }, t).unwrap();
            }
            assert_eq!(w.finish().unwrap().len(), 2);
        });
        let files = tarball(&r.ctrl, "/recordings.tar");
        let manifest = String::from_utf8(
            files
                .get("captured/codec2-3200/manifest.jsonl")
                .expect("the manifest")
                .clone(),
        )
        .unwrap();
        let rows: Vec<astar_record::capture::CaptureRow> = manifest
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        for row in rows {
            let stem = row.key.strip_prefix("astar-air/").unwrap();
            for ext in ["ambe", "flac"] {
                let p = format!("captured/codec2-3200/astar-air/{stem}.{ext}");
                assert!(
                    files.contains_key(&p),
                    "{p} missing from {:?}",
                    files.keys()
                );
            }
            let ambe = &files[&format!("captured/codec2-3200/astar-air/{stem}.ambe")];
            assert_eq!(ambe.len(), 40);
        }
        // The ledgers come after the files they list.
        let (_, body) = fetch(&r.ctrl, "/recordings.tar");
        let (tar, _) = dechunk(&body);
        let mut a = tar::Archive::new(tar.as_slice());
        let order: Vec<String> = a
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(order.last().unwrap(), "captured/codec2-3200/manifest.jsonl");
        assert!(order.iter().any(|p| p == "recordings/index.jsonl"));
    }

    /// A download cut by a file shrinking under it: the client reads the
    /// head and a little, the file is truncated while the server is still
    /// copying it, and the rest is read to the close.
    fn cut_short(r: &Rig, target: &str) -> (String, Vec<u8>) {
        cut_short_with(r, target, "")
    }

    fn cut_short_with(r: &Rig, target: &str, extra: &str) -> (String, Vec<u8>) {
        let big = r.dir.path().join(&r.newer.path);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&big)
            .unwrap()
            .set_len(64 << 20)
            .unwrap();
        let server = crate::server::bind_control(&r.ctrl, "127.0.0.1:0").unwrap();
        let mut s = TcpStream::connect(server.local_addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        write!(
            s,
            "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut all = vec![0u8; 256 * 1024];
        s.read_exact(&mut all).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&big)
            .unwrap()
            .set_len(0)
            .unwrap();
        let _ = s.read_to_end(&mut all);
        server.stop();
        split(&all)
    }

    #[test]
    fn a_tar_cut_short_never_sends_its_last_chunk() {
        let r = rig();
        let (head, body) = cut_short(&r, "/recordings.tar");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let (tar, complete) = dechunk(&body);
        assert!(!complete, "an aborted tar must not end like a whole one");
        assert!(tar.len() < 64 << 20, "{}", tar.len());
    }

    #[test]
    fn a_file_cut_short_falls_short_of_its_length() {
        let r = rig();
        let (head, body) = cut_short(&r, "/recordings/file?name=seed-newer.flac");
        assert!(
            head.contains(&format!("Content-Length: {}\r\n", 64 << 20)),
            "{head}"
        );
        assert!(body.len() < 64 << 20, "{}", body.len());
    }

    /// A client that goes away after 100 bytes.
    struct Fails(Vec<u8>);
    impl Write for Fails {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.0.len() > 100 {
                return Err(io::Error::other("client gone"));
            }
            self.0.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn send_tar_ends_with_the_last_chunk_only_when_whole() {
        let mut out = Vec::new();
        send_tar(&mut out, &DownloadPlan::default()).unwrap();
        let (tar, complete) = dechunk(&out);
        assert!(complete);
        assert!(untar(&tar).is_empty());
        // A write that fails midway: no last chunk.
        let r = rig();
        let plan = r.logger.plan_download(None);
        let mut f = Fails(Vec::new());
        assert!(send_tar(&mut f, &plan).is_err());
        assert!(!dechunk(&f.0).1);
    }

    #[test]
    fn a_range_is_answered_206_with_exactly_those_bytes() {
        let r = rig();
        let pattern: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(r.dir.path().join(&r.newer.path), &pattern).unwrap();
        let target = "/recordings/file?name=seed-newer.flac";
        let (head, body) = fetch(&r.ctrl, target);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Accept-Ranges: bytes\r\n"), "{head}");
        assert_eq!(body, pattern);
        for (range, start, end) in [
            ("bytes=0-1", 0usize, 1usize),
            ("bytes=500-999", 500, 999),
            ("bytes=-100", 1900, 1999),
            ("bytes=1500-", 1500, 1999),
            ("bytes=1999-5000", 1999, 1999),
            ("bytes=-5000", 0, 1999),
        ] {
            let (head, body) = fetch_with(&r.ctrl, target, &format!("Range: {range}\r\n"));
            assert!(
                head.starts_with("HTTP/1.1 206 Partial Content"),
                "{range}: {head}"
            );
            assert!(
                head.contains(&format!("Content-Range: bytes {start}-{end}/2000\r\n")),
                "{range}: {head}"
            );
            assert!(
                head.contains(&format!("Content-Length: {}\r\n", end - start + 1)),
                "{range}: {head}"
            );
            assert!(head.contains("Accept-Ranges: bytes\r\n"), "{range}: {head}");
            assert_eq!(body, &pattern[start..=end], "{range}");
        }
        for range in ["bytes=2000-", "bytes=5000-6000", "bytes=-0"] {
            let (head, body) = fetch_with(&r.ctrl, target, &format!("Range: {range}\r\n"));
            assert!(
                head.starts_with("HTTP/1.1 416 Range Not Satisfiable"),
                "{range}: {head}"
            );
            assert!(
                head.contains("Content-Range: bytes */2000\r\n"),
                "{range}: {head}"
            );
            assert!(body.len() < 100, "{range}");
        }
        // More than one range, or one that does not parse: the whole file.
        for range in ["bytes=0-1,5-6", "bytes=5-2", "items=0-1", "bytes=x-"] {
            let (head, body) = fetch_with(&r.ctrl, target, &format!("Range: {range}\r\n"));
            assert!(head.starts_with("HTTP/1.1 200"), "{range}: {head}");
            assert_eq!(body, pattern, "{range}");
        }
    }

    #[test]
    fn a_range_cut_short_falls_short_of_its_length() {
        let r = rig();
        let (head, body) = cut_short_with(
            &r,
            "/recordings/file?name=seed-newer.flac",
            "Range: bytes=1000-\r\n",
        );
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        let want = (64u64 << 20) - 1000;
        assert!(
            head.contains(&format!("Content-Length: {want}\r\n")),
            "{head}"
        );
        assert!((body.len() as u64) < want, "{}", body.len());
    }

    #[test]
    fn ranges_parse_as_one_satisfiable_span_or_nothing() {
        assert_eq!(parse_range("bytes=0-1", 10), Some(Ok((0, 1))));
        assert_eq!(parse_range("bytes=-3", 10), Some(Ok((7, 9))));
        assert_eq!(parse_range("bytes=4-", 10), Some(Ok((4, 9))));
        assert_eq!(parse_range("bytes=4-99", 10), Some(Ok((4, 9))));
        assert_eq!(parse_range(" bytes = 2 - 3 ", 10), Some(Ok((2, 3))));
        assert_eq!(parse_range("Bytes=0-1", 10), Some(Ok((0, 1))));
        assert_eq!(parse_range("BYTES=-2", 10), Some(Ok((8, 9))));
        assert_eq!(parse_range("bytes=10-", 10), Some(Err(())));
        assert_eq!(parse_range("bytes=-0", 10), Some(Err(())));
        assert_eq!(parse_range("bytes=0-", 0), Some(Err(())));
        for ignored in [
            "bytes=0-1,2-3",
            "bytes=3-2",
            "bytes=-",
            "bytes=",
            "bytes=+1-2",
            "lines=0-1",
        ] {
            assert_eq!(parse_range(ignored, 10), None, "{ignored}");
        }
    }

    #[test]
    fn percent_escapes_need_two_hex_digits() {
        assert_eq!(percent_decode("a%2Fb").as_deref(), Some("a/b"));
        assert_eq!(percent_decode("%2e%2E").as_deref(), Some(".."));
        for bad in ["%+1", "%-1", "% 1", "%1", "%", "%zz", "%1g"] {
            assert_eq!(percent_decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn since_may_carry_fractional_seconds() {
        let r = rig();
        let got = download_for(
            &r.ctrl,
            "GET",
            "/recordings.tar",
            "since=2026-10-27T01%3A00%3A04.250Z",
        );
        let want = parse_rfc3339("2026-10-27T01:00:04Z");
        assert_eq!(got, Some(Ok(Download::Tar(want))));
    }

    /// The public bind's `/admin/` group, signed in: the downloads stream
    /// there too, a viewer is refused them, and a delete needs a
    /// same-origin JSON POST.
    #[cfg(feature = "auth")]
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one rig, every route of the group in turn"
    )]
    fn the_public_bind_streams_downloads_to_an_admin_only() {
        use crate::auth::store::Role;
        use crate::public::guard::{Guard, Limits};
        use crate::public::http::PublicCtx;
        use crate::public::status::Activity;
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;

        const HOST: &str = "node.example.net";
        let r = rig();
        let state = tempfile::tempdir().unwrap();
        let auth = Arc::new(crate::auth::AuthService::open(
            crate::auth::AuthSettings::defaults(state.path()),
        ));
        r.ctrl.set_auth(Arc::clone(&auth));
        let admin = format!("astar_session={}", auth.test_sign_in("A1DM", Role::Admin));
        let viewer = format!("astar_session={}", auth.test_sign_in("V1EW", Role::Viewer));
        let ctx = Arc::new(PublicCtx {
            ctrl: Arc::clone(&r.ctrl),
            hub: None,
            activity: Arc::new(Mutex::new(Activity::new(std::time::Instant::now()))),
            hostnames: vec![HOST.to_string()],
            tls: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            anonymous_status: true,
            guard: Guard::new(Limits::default()),
        });
        let challenges = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let l =
            crate::public::serve::Listeners::start("127.0.0.1:0", None, &ctx, &challenges, None)
                .unwrap();
        let addr = l.public_addr();
        let send = |raw: String| {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            s.write_all(raw.as_bytes()).unwrap();
            let mut all = Vec::new();
            let _ = s.read_to_end(&mut all);
            split(&all)
        };
        let get = |path: &str, cookie: &str| {
            send(format!(
                "GET {path} HTTP/1.1\r\nHost: {HOST}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ))
        };

        let (head, body) = get("/admin/recordings.tar", &admin);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let (tar, complete) = dechunk(&body);
        assert!(complete);
        assert!(untar(&tar).contains_key(&format!("recordings/{}", r.newer.path)));
        let (head, body) = get("/admin/recordings/file?name=seed-older.flac", &admin);
        assert!(head.contains("Content-Length: 1000\r\n"), "{head}");
        assert_eq!(body, vec![b'f'; 1000]);
        let (head, body) = send(format!(
            "GET /admin/recordings/file?name=seed-older.flac HTTP/1.1\r\nHost: {HOST}\r\nCookie: {admin}\r\nRange: bytes=10-19\r\nConnection: close\r\n\r\n"
        ));
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        assert!(
            head.contains("Content-Range: bytes 10-19/1000\r\n"),
            "{head}"
        );
        assert_eq!(body, vec![b'f'; 10]);
        let (head, _) = get("/admin/recordings/file?name=..%2Fx", &admin);
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        for path in [
            "/admin/recordings.tar",
            "/admin/recordings/file?name=seed-older.flac",
        ] {
            let (head, body) = get(path, &viewer);
            assert!(head.starts_with("HTTP/1.1 403"), "{path}: {head}");
            assert!(String::from_utf8_lossy(&body).contains("admins_only"));
            let (head, _) = get(path, "");
            assert!(head.starts_with("HTTP/1.1 401"), "{path}: {head}");
        }

        let post = |cookie: &str, extra: &str, ctype: &str| {
            let body = r#"{"name":"seed-older.flac"}"#;
            send(format!(
                "POST /admin/recordings/delete HTTP/1.1\r\nHost: {HOST}\r\nCookie: {cookie}\r\n{extra}Content-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ))
        };
        let (head, _) = post(&viewer, "", "application/json");
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        let (head, _) = post(
            &admin,
            "Origin: https://evil.example\r\n",
            "application/json",
        );
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        let (head, _) = post(&admin, "Sec-Fetch-Site: cross-site\r\n", "text/plain");
        assert!(head.starts_with("HTTP/1.1 415"), "{head}");
        assert_eq!(
            r.logger.rows().len(),
            2,
            "nothing deleted by a refused POST"
        );
        let (head, body) = post(
            &admin,
            &format!("Origin: http://{HOST}\r\n"),
            "application/json",
        );
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, br#"{"deleted":true}"#);
        assert_eq!(r.logger.rows().len(), 1);
        l.stop();
    }
}
