// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The admin Recordings page's routes: the logger's state, deleting one
//! recording, and the two downloads (one FLAC, or every recording and
//! capture as a tar). The packet logs listed on that page have routes of
//! their own here: the state, one delete and one file download.
//!
//! Every route is admin-class (`crate::access::ADMIN_ROUTES`), so a viewer
//! or an anonymous caller never reaches this module. A file is always named
//! by its index row's `name`, never by a path: the path served or deleted
//! is the row's own. A packet log has no index, so its name must equal,
//! byte for byte, a name in the folder's current listing. That comparison
//! is the whole check: a disk that ignores case would open a variant
//! spelling, and the variant would miss the station's open-file check.
//! With no `[packet_log]` table these routes say `packet log is off`; with
//! one they work whether or not a capture is running, because the logs of an
//! earlier one hold the same voices and an admin still has to reach them.
//!
//! Listing and deleting a packet log read the disk and wait out a change to
//! the log. Every handler here runs on its connection's own thread
//! (`astar_http`, a thread per connection) and no async executor is
//! involved, so that wait costs one connection and nothing else.
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

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use astar_http::{Chunked, Reply, Response, StreamClass};
use astar_record::clock::parse_rfc3339;
use astar_record::packet_log::COMPANION_SUFFIX;
use astar_record::tarball::{DownloadPlan, TarSummary, open_regular, write_tar_planned};

use astar_station::DeleteRefused;

use crate::controller::{NodeController, PacketCaptureRefused};
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
    /// A packet log file inside the folder, and the name it was listed as.
    /// Always an attachment.
    PacketLog(PathBuf, String),
}

fn off() -> HttpResponse {
    HttpResponse::error_json(404, "recording is off")
}

fn no_such_recording() -> HttpResponse {
    HttpResponse::error_json(404, "no such recording")
}

fn packet_log_off() -> HttpResponse {
    HttpResponse::error_json(404, "packet log is off")
}

fn no_such_packet_log() -> HttpResponse {
    HttpResponse::error_json(404, "no such packet log")
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

/// `GET /packet-logs/state`: `{"debug_mode": …, "state": …, "rows": […]}`,
/// rows newest first. A capture that can no longer write ends here, so what
/// this answers is true. Runs on the connection's own thread, because ending
/// one joins the writer.
#[must_use]
pub fn packet_log_state(ctrl: &NodeController) -> HttpResponse {
    if ctrl.packet_log_dir().is_none() {
        return packet_log_off();
    }
    ctrl.stop_failed_capture();
    no_store(HttpResponse::json(
        200,
        &serde_json::json!({
            "debug_mode": ctrl.packet_debug_mode(),
            "state": ctrl.packet_log_status(),
            "rows": ctrl.packet_log_rows(),
        }),
    ))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureRequest {
    capturing: bool,
}

/// `POST /packet-logs/capture` with `{"capturing": true|false}`: the state
/// the caller wants, not a toggle, so two tabs can't undo each other. 404
/// with no `[packet_log]` table, 409 with `debug_mode` off, 503 when the
/// folder can't be used. Stopping is never refused. The caller has already
/// checked that this is a same-origin JSON request. Runs on the connection's
/// own thread, because starting may join the writer.
#[must_use]
pub fn packet_capture(ctrl: &NodeController, body: &[u8]) -> HttpResponse {
    let req: CaptureRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
    };
    match ctrl.packet_capture(req.capturing) {
        Ok(()) => no_store(HttpResponse::json(
            200,
            &serde_json::json!({ "capturing": req.capturing }),
        )),
        Err(PacketCaptureRefused::NoTable) => packet_log_off(),
        Err(PacketCaptureRefused::DebugModeOff) => {
            HttpResponse::error_json(409, "debug mode is off")
        }
        Err(PacketCaptureRefused::Station(message)) => {
            tracing::warn!("packet log: capture refused: {message}");
            HttpResponse::error_json(
                503,
                "Couldn't start the capture. Check the packet log folder and its free space.",
            )
        }
    }
}

/// `POST /packet-logs/delete` with `{"name": "<pcap name>"}`: the log's two
/// files, or 404 unless that exact name is listed, or 409 while a call is
/// writing to it. The caller has already checked that this is a same-origin
/// JSON request.
#[must_use]
pub fn packet_log_delete(ctrl: &NodeController, body: &[u8]) -> HttpResponse {
    if ctrl.packet_log_dir().is_none() {
        return packet_log_off();
    }
    let req: DeleteRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
    };
    if !ctrl.packet_log_rows().iter().any(|r| r.name == req.name) {
        return no_such_packet_log();
    }
    delete_answer(ctrl.packet_log_delete(&req.name))
}

/// What a client hears of a delete. An I/O failure's text names the folder,
/// so it goes to the log, not to the page.
fn delete_answer(result: Result<(), DeleteRefused>) -> HttpResponse {
    match result {
        Ok(()) => HttpResponse::json(200, &serde_json::json!({ "deleted": true })),
        Err(DeleteRefused::NoSuchLog) => no_such_packet_log(),
        Err(DeleteRefused::Open) => {
            HttpResponse::error_json(409, "that call is still being logged")
        }
        Err(DeleteRefused::Io(e)) => {
            tracing::warn!("packet log: delete failed: {e}");
            HttpResponse::error_json(500, "cannot delete that log")
        }
    }
}

/// The download of a listed pcap or companion, or the 404 that refuses it.
fn packet_log_download(ctrl: &NodeController, query: &str) -> Result<Download, HttpResponse> {
    let Some(dir) = ctrl.packet_log_dir() else {
        return Err(packet_log_off());
    };
    let name = query_value(query, "name").unwrap_or_default();
    let listed = ctrl
        .packet_log_rows()
        .iter()
        .any(|r| r.name == name || r.companion == name);
    if !listed {
        return Err(no_such_packet_log());
    }
    Ok(Download::PacketLog(dir.join(&name), name))
}

/// The download a request asks for: `None` when `(method, admin_path)` is
/// not a download route, `Some(Err(…))` for the answer that refuses it
/// (recording or the packet log off, an unknown name, a bad `since`).
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
        ("GET", "/packet-logs/file") => return Some(packet_log_download(ctrl, query)),
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
                let how = FileReply {
                    content_type: "audio/flac",
                    attach,
                    parent_may_link: false,
                    gone: no_such_recording(),
                };
                run_file(conn, &path, &name, &how, range.as_deref());
            }
            Download::Tar(since) => run_tar(conn, &ctrl, since),
            Download::PacketLog(path, name) => {
                let how = FileReply {
                    content_type: packet_log_type(&name),
                    attach: true,
                    parent_may_link: true,
                    gone: no_such_packet_log(),
                };
                run_file(conn, &path, &name, &how, range.as_deref());
            }
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

/// How [`run_file`] answers for one kind of file.
struct FileReply {
    content_type: &'static str,
    attach: bool,
    /// The parent is the operator's configured folder, which may be a link,
    /// not a day folder inside it. The file itself is never followed.
    parent_may_link: bool,
    /// The answer when the file can't be opened.
    gone: HttpResponse,
}

fn packet_log_type(name: &str) -> &'static str {
    if name.ends_with(COMPANION_SUFFIX) {
        "application/x-ndjson"
    } else {
        "application/vnd.tcpdump.pcap"
    }
}

/// `name` as a `filename` parameter. A packet log's name can carry a label
/// off the wire, so anything that could end the quoted string, break the
/// header or read differently from what it is becomes `_`.
fn attachment_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn run_file(conn: astar_http::Conn, path: &Path, name: &str, how: &FileReply, range: Option<&str>) {
    // Opened only now, and never through a symlink: the file or the folder
    // it sits in may have changed since the name was looked up. A recording
    // sits in a day folder, checked here; a packet log sits in the folder
    // the operator chose, which `how.parent_may_link` lets be a link.
    let parent_ok = how.parent_may_link
        || path
            .parent()
            .is_some_and(|d| std::fs::symlink_metadata(d).is_ok_and(|m| m.file_type().is_dir()));
    let opened = if parent_ok {
        open_regular(path).ok().flatten()
    } else {
        None
    };
    let Some((file, size)) = opened else {
        respond(conn, &how.gone);
        return;
    };
    let disposition = format!("attachment; filename=\"{}\"", attachment_name(name));
    let mut headers = vec![
        ("Content-Type", how.content_type),
        ("Cache-Control", "no-store"),
    ];
    if how.attach {
        headers.push(("Content-Disposition", disposition.as_str()));
    }
    // The span comes from the size of the open handle, and the seek is on
    // that same handle: nothing is re-opened by path.
    astar_http::files::send_file(conn, file, size, &headers, range, name);
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
    use crate::test_support::{packet_log_cfg, seed_packet_log};
    use std::collections::BTreeMap;
    use std::io::Read;
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

    // ---- the packet log ---------------------------------------------------

    struct LogRig {
        ctrl: Arc<NodeController>,
        /// The packet log folder, inside `root`.
        dir: PathBuf,
        root: tempfile::TempDir,
        /// A closed log's pcap name.
        name: String,
    }

    const PCAP_BYTES: &[u8] = b"\xd4\xc3\xb2\xa1 a capture";

    /// A capture running over `dir`.
    fn start_capture(ctrl: &NodeController, dir: &Path) {
        ctrl.set_packet_log_settings(Some(&packet_log_cfg(dir)));
        ctrl.packet_capture(true).unwrap();
    }

    /// A capture running over `<root>/logs`, with one closed log in it.
    fn log_rig() -> LogRig {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("logs");
        std::fs::create_dir(&dir).unwrap();
        let ctrl = controller();
        start_capture(&ctrl, &dir);
        let name = seed_packet_log(&dir, "20261105-call", PCAP_BYTES);
        LogRig {
            ctrl,
            dir,
            root,
            name,
        }
    }

    fn delete_log_named(ctrl: &NodeController, name: &str) -> HttpResponse {
        let body = serde_json::json!({ "name": name }).to_string();
        packet_log_delete(ctrl, body.as_bytes())
    }

    fn log_file(ctrl: &NodeController, name: &str) -> Option<Result<Download, HttpResponse>> {
        let q = format!("name={}", percent_encode(name));
        download_for(ctrl, "GET", "/packet-logs/file", &q)
    }

    #[test]
    fn packet_log_state_gives_the_state_and_the_rows() {
        let r = log_rig();
        let resp = packet_log_state(&r.ctrl);
        assert_eq!(resp.status, 200);
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| *k == "Cache-Control" && v == "no-store"),
            "{resp:?}"
        );
        let v = body_json(&resp);
        assert_eq!(v["state"]["capturing"], true, "{v}");
        assert_eq!(v["rows"][0]["name"], r.name.as_str(), "{v}");
        assert_eq!(v["rows"][0]["companion"], "20261105-call.jitter.jsonl");
        assert_eq!(v["rows"][0]["open"], false);
        assert!(v["rows"][0]["bytes"].as_u64().unwrap() > 0, "{v}");
    }

    #[test]
    fn state_reports_debug_mode() {
        let r = log_rig();
        assert_eq!(body_json(&packet_log_state(&r.ctrl))["debug_mode"], true);
        r.ctrl
            .set_packet_log_settings(Some(&crate::config::PacketLogCfg {
                debug_mode: false,
                ..packet_log_cfg(&r.dir)
            }));
        assert_eq!(body_json(&packet_log_state(&r.ctrl))["debug_mode"], false);
    }

    #[test]
    fn packet_log_routes_say_off_when_logging_is_off() {
        // No `[packet_log]` table at all.
        let ctrl = controller();
        let is_off = |r: &HttpResponse| {
            assert_eq!(
                (r.status, body_json(r)["error"].clone()),
                (404, "packet log is off".into())
            );
        };
        is_off(&packet_log_state(&ctrl));
        is_off(&delete_log_named(&ctrl, "x.pcap"));
        let Some(Err(resp)) = log_file(&ctrl, "x.pcap") else {
            panic!("off refuses the file");
        };
        is_off(&resp);
        let (head, body) = fetch(&ctrl, "/packet-logs/file?name=x.pcap");
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains("packet log is off"));
    }

    /// Debug mode off over `<root>/logs`, with one log from an earlier run.
    fn stopped_log_rig() -> LogRig {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("logs");
        std::fs::create_dir(&dir).unwrap();
        let ctrl = controller();
        ctrl.set_packet_log_settings(Some(&crate::config::PacketLogCfg {
            debug_mode: false,
            ..packet_log_cfg(&dir)
        }));
        let name = seed_packet_log(&dir, "20261105-call", PCAP_BYTES);
        LogRig {
            ctrl,
            dir,
            root,
            name,
        }
    }

    #[test]
    fn a_table_with_capture_off_still_lists_downloads_and_deletes() {
        let r = stopped_log_rig();
        let resp = packet_log_state(&r.ctrl);
        assert_eq!(resp.status, 200, "{resp:?}");
        let v = body_json(&resp);
        assert_eq!(v["state"]["capturing"], false, "{v}");
        assert_eq!(v["rows"][0]["name"], r.name.as_str(), "{v}");
        assert_eq!(v["rows"][0]["open"], false, "{v}");

        let (head, body) = fetch(&r.ctrl, &format!("/packet-logs/file?name={}", r.name));
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, PCAP_BYTES);
        let (head, _) = fetch(&r.ctrl, "/packet-logs/file?name=20261105-call.jitter.jsonl");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");

        // The same naming rule with no logger behind it.
        std::fs::write(r.root.path().join("x"), b"outside").unwrap();
        std::fs::write(r.dir.join("foreign.pcap"), b"someone else's").unwrap();
        for bad in ["../x", "foreign.pcap", "20261105-CALL.pcap", ""] {
            let got = log_file(&r.ctrl, bad);
            assert!(
                matches!(got, Some(Err(HttpResponse { status: 404, .. }))),
                "{bad:?}: {got:?}"
            );
            assert_eq!(delete_log_named(&r.ctrl, bad).status, 404, "{bad:?}");
        }
        assert!(r.dir.join(&r.name).exists());

        assert_eq!(delete_log_named(&r.ctrl, &r.name).status, 200);
        assert!(!r.dir.join(&r.name).exists());
        assert!(!r.dir.join("20261105-call.jitter.jsonl").exists());
        assert!(r.dir.join("foreign.pcap").exists());
        assert_eq!(delete_log_named(&r.ctrl, &r.name).status, 404);
    }

    #[test]
    fn a_refused_start_is_off_and_says_why() {
        let ctrl = controller();
        let not_a_folder = tempfile::NamedTempFile::new().unwrap();
        ctrl.set_packet_log_settings(Some(&packet_log_cfg(not_a_folder.path())));
        ctrl.packet_capture(true).unwrap_err();
        let resp = packet_log_state(&ctrl);
        assert_eq!(resp.status, 200, "{resp:?}");
        let v = body_json(&resp);
        assert_eq!(v["state"]["capturing"], false, "{v}");
        assert!(v["state"]["error"].is_string(), "{v}");
        assert_eq!(v["rows"], serde_json::json!([]), "{v}");
    }

    /// The folder is the operator's config, so a link there is fine; a file
    /// inside it is still opened without following.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_folder_downloads_and_a_symlinked_file_inside_it_does_not() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let ctrl = controller();
        start_capture(&ctrl, &link);
        let name = seed_packet_log(&real, "call", PCAP_BYTES);
        let (head, body) = fetch(&ctrl, &format!("/packet-logs/file?name={name}"));
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, PCAP_BYTES);

        let secret = root.path().join("secret");
        std::fs::write(&secret, b"not a log").unwrap();
        seed_packet_log(&real, "linked", b"");
        std::fs::remove_file(real.join("linked.pcap")).unwrap();
        std::os::unix::fs::symlink(&secret, real.join("linked.pcap")).unwrap();
        let (head, body) = fetch(&ctrl, "/packet-logs/file?name=linked.pcap");
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains("no such packet log"));
    }

    #[test]
    fn packet_log_files_are_named_by_listing_never_by_path() {
        let r = log_rig();
        // Every one of these is a real file, so only the listing refuses it.
        std::fs::write(r.root.path().join("x"), b"outside").unwrap();
        std::fs::write(r.dir.join("foreign.pcap"), b"someone else's").unwrap();
        std::fs::create_dir(r.dir.join("a")).unwrap();
        seed_packet_log(&r.dir.join("a"), "b", b"nested");
        let absolute = r.dir.join(&r.name).to_string_lossy().into_owned();
        for bad in [
            "../x",
            "/etc/passwd",
            "a/b.pcap",
            "foreign.pcap",
            "..",
            ".",
            "",
            absolute.as_str(),
        ] {
            let got = log_file(&r.ctrl, bad);
            let Some(Err(resp)) = got else {
                panic!("{bad:?}: {got:?}");
            };
            assert_eq!(resp.status, 404, "{bad:?}");
            assert_eq!(body_json(&resp)["error"], "no such packet log", "{bad:?}");
            assert_eq!(delete_log_named(&r.ctrl, bad).status, 404, "{bad:?}");
        }
        // Percent-encoded on the wire, as a client would send them.
        for bad in ["..%2fx", "..%2Fx", "%2e%2e%2fx", "%zz", "a%2fb.pcap"] {
            let target = format!("/packet-logs/file?name={bad}");
            let (head, body) = fetch(&r.ctrl, &target);
            assert!(head.starts_with("HTTP/1.1 404"), "{bad}: {head}");
            assert!(String::from_utf8_lossy(&body).contains("no such packet log"));
        }
        assert!(
            matches!(
                download_for(&r.ctrl, "GET", "/packet-logs/file", ""),
                Some(Err(HttpResponse { status: 404, .. }))
            ),
            "no name at all"
        );
        assert_eq!(std::fs::read(r.dir.join(&r.name)).unwrap(), PCAP_BYTES);
        assert_eq!(
            std::fs::read(r.dir.join("foreign.pcap")).unwrap(),
            b"someone else's"
        );
        assert!(r.dir.join("a/b.pcap").exists());
        assert!(r.root.path().join("x").exists());
        // The listed names resolve inside the folder.
        assert_eq!(
            log_file(&r.ctrl, &r.name),
            Some(Ok(Download::PacketLog(r.dir.join(&r.name), r.name.clone())))
        );
        assert_eq!(
            log_file(&r.ctrl, "20261105-call.jitter.jsonl"),
            Some(Ok(Download::PacketLog(
                r.dir.join("20261105-call.jitter.jsonl"),
                "20261105-call.jitter.jsonl".into()
            )))
        );
        assert_eq!(download_for(&r.ctrl, "POST", "/packet-logs/file", ""), None);
    }

    /// On a case-insensitive disk the variant opens the same file, and for a
    /// log that's still open the path no longer matches the open one.
    #[test]
    fn a_case_variant_name_is_404_on_file_and_on_delete() {
        let r = log_rig();
        for variant in [
            "20261105-CALL.pcap",
            "20261105-Call.pcap",
            "20261105-CALL.jitter.jsonl",
            "20261105-call.PCAP",
        ] {
            let got = log_file(&r.ctrl, variant);
            let Some(Err(resp)) = got else {
                panic!("{variant}: {got:?}");
            };
            assert_eq!(resp.status, 404, "{variant}");
            assert_eq!(delete_log_named(&r.ctrl, variant).status, 404, "{variant}");
        }
        assert_eq!(std::fs::read(r.dir.join(&r.name)).unwrap(), PCAP_BYTES);
    }

    #[test]
    fn a_pcap_downloads_as_an_attachment_with_its_type() {
        let r = log_rig();
        let (head, body) = fetch(&r.ctrl, &format!("/packet-logs/file?name={}", r.name));
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            head.contains("Content-Type: application/vnd.tcpdump.pcap\r\n"),
            "{head}"
        );
        assert!(
            head.contains(&format!("Content-Length: {}\r\n", PCAP_BYTES.len())),
            "{head}"
        );
        assert!(
            head.contains(&format!(
                "Content-Disposition: attachment; filename=\"{}\"\r\n",
                r.name
            )),
            "{head}"
        );
        assert!(head.contains("Cache-Control: no-store\r\n"), "{head}");
        assert_eq!(body, PCAP_BYTES);

        let companion = "20261105-call.jitter.jsonl";
        let (head, body) = fetch(
            &r.ctrl,
            &format!("/admin/packet-logs/file?name={companion}"),
        );
        assert!(
            head.contains("Content-Type: application/x-ndjson\r\n"),
            "{head}"
        );
        assert!(
            head.contains(&format!(
                "Content-Disposition: attachment; filename=\"{companion}\"\r\n"
            )),
            "{head}"
        );
        assert_eq!(body, std::fs::read(r.dir.join(companion)).unwrap());

        // The page's probe before it starts a download.
        let (head, body) = fetch_with(
            &r.ctrl,
            &format!("/packet-logs/file?name={}", r.name),
            "Range: bytes=0-0\r\n",
        );
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        assert_eq!(body, &PCAP_BYTES[..1]);
    }

    #[test]
    fn a_name_with_odd_characters_makes_a_safe_header() {
        let r = log_rig();
        let hostile = "a\"b\r\nSet-Cookie: x=1; \\\u{202e}gpj";
        let name = seed_packet_log(&r.dir, hostile, PCAP_BYTES);
        let target = format!("/packet-logs/file?name={}", percent_encode(&name));
        let (head, body) = fetch(&r.ctrl, &target);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, PCAP_BYTES, "the odd name still finds its file");
        assert!(!head.contains("Set-Cookie:"), "{head}");
        assert!(head.is_ascii(), "{head}");
        let line = head
            .lines()
            .find(|l| l.starts_with("Content-Disposition:"))
            .expect("a disposition");
        let value = line
            .strip_prefix("Content-Disposition: attachment; filename=\"")
            .and_then(|l| l.strip_suffix('"'))
            .expect("one quoted value");
        assert!(!value.contains(['"', '\\', '\r', '\n', '/']), "{value:?}");
        assert!(
            Path::new(value).extension().is_some_and(|e| e == "pcap"),
            "{value:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_listed_symlink_is_never_followed() {
        let r = log_rig();
        let secret = r.root.path().join("secret");
        std::fs::write(&secret, b"not a log").unwrap();
        seed_packet_log(&r.dir, "linked", b"");
        std::fs::remove_file(r.dir.join("linked.pcap")).unwrap();
        std::os::unix::fs::symlink(&secret, r.dir.join("linked.pcap")).unwrap();
        let (head, body) = fetch(&r.ctrl, "/packet-logs/file?name=linked.pcap");
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains("no such packet log"));
    }

    #[test]
    fn packet_log_delete_removes_both_files_and_takes_only_a_pcap_name() {
        let r = log_rig();
        let companion = "20261105-call.jitter.jsonl";
        assert_eq!(delete_log_named(&r.ctrl, companion).status, 404);
        assert!(r.dir.join(&r.name).exists());
        let resp = delete_log_named(&r.ctrl, &r.name);
        assert_eq!(resp.status, 200);
        assert_eq!(body_json(&resp), serde_json::json!({ "deleted": true }));
        assert!(!r.dir.join(&r.name).exists());
        assert!(!r.dir.join(companion).exists());
        assert_eq!(delete_log_named(&r.ctrl, &r.name).status, 404);
        assert_eq!(packet_log_delete(&r.ctrl, b"not json").status, 400);
    }

    #[test]
    fn a_listed_log_gone_before_the_delete_is_a_404_and_a_failure_says_nothing_of_the_path() {
        assert_eq!(delete_answer(Err(DeleteRefused::NoSuchLog)).status, 404);
        assert_eq!(delete_answer(Err(DeleteRefused::Open)).status, 409);
        let failed = delete_answer(Err(DeleteRefused::Io("/var/secret: EPERM".into())));
        assert_eq!(failed.status, 500);
        assert!(!String::from_utf8_lossy(&failed.body).contains("secret"));
    }

    /// Signed-in sessions over the public bind's `/admin/` group.
    #[cfg(feature = "auth")]
    struct PublicBind {
        listeners: crate::public::serve::Listeners,
        admin: String,
        viewer: String,
        _state: tempfile::TempDir,
    }

    #[cfg(feature = "auth")]
    const HOST: &str = "node.example.net";

    #[cfg(feature = "auth")]
    fn public_bind(ctrl: &Arc<NodeController>) -> PublicBind {
        use crate::auth::store::Role;
        use crate::public::guard::{Guard, Limits};
        use crate::public::http::PublicCtx;
        use crate::public::status::Activity;
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;

        let state = tempfile::tempdir().unwrap();
        let auth = Arc::new(crate::auth::AuthService::open(
            crate::auth::AuthSettings::defaults(state.path()),
        ));
        ctrl.set_auth(Arc::clone(&auth));
        let admin = format!("astar_session={}", auth.test_sign_in("A1DM", Role::Admin));
        let viewer = format!("astar_session={}", auth.test_sign_in("V1EW", Role::Viewer));
        let ctx = Arc::new(PublicCtx {
            ctrl: Arc::clone(ctrl),
            hub: None,
            activity: Arc::new(Mutex::new(Activity::new(std::time::Instant::now()))),
            hostnames: vec![HOST.to_string()],
            tls: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            anonymous_status: true,
            guard: Guard::new(Limits::default()),
        });
        let challenges = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let listeners =
            crate::public::serve::Listeners::start("127.0.0.1:0", None, &ctx, &challenges, None)
                .unwrap();
        PublicBind {
            listeners,
            admin,
            viewer,
            _state: state,
        }
    }

    #[cfg(feature = "auth")]
    impl PublicBind {
        fn send(&self, raw: &str) -> (String, Vec<u8>) {
            let mut s = TcpStream::connect(self.listeners.public_addr()).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            s.write_all(raw.as_bytes()).unwrap();
            let mut all = Vec::new();
            let _ = s.read_to_end(&mut all);
            split(&all)
        }

        fn get(&self, path: &str, cookie: &str) -> (String, Vec<u8>) {
            self.send(&format!(
                "GET {path} HTTP/1.1\r\nHost: {HOST}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ))
        }
    }

    /// The same rules as `/recordings`: an admin only, and a delete needs a
    /// same-origin JSON POST.
    #[cfg(feature = "auth")]
    #[test]
    fn the_public_bind_serves_packet_logs_to_an_admin_only() {
        let r = log_rig();
        let b = public_bind(&r.ctrl);
        let file = format!("/admin/packet-logs/file?name={}", r.name);

        let (head, body) = b.get("/admin/packet-logs/state", &b.admin);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains(&r.name));
        let (head, body) = b.get(&file, &b.admin);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Content-Disposition: attachment;"), "{head}");
        assert!(
            head.contains("X-Content-Type-Options: nosniff\r\n"),
            "{head}"
        );
        assert_eq!(body, PCAP_BYTES);
        let (head, _) = b.get("/admin/packet-logs/file?name=..%2fx", &b.admin);
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");

        for path in ["/admin/packet-logs/state", file.as_str()] {
            let (head, body) = b.get(path, &b.viewer);
            assert!(head.starts_with("HTTP/1.1 403"), "{path}: {head}");
            assert!(String::from_utf8_lossy(&body).contains("admins_only"));
            let (head, body) = b.get(path, "");
            assert!(head.starts_with("HTTP/1.1 401"), "{path}: {head}");
            assert!(!body.windows(PCAP_BYTES.len()).any(|w| w == PCAP_BYTES));
        }
        // Outside the prefix there is no such thing on the public bind.
        let (head, _) = b.get(&format!("/packet-logs/file?name={}", r.name), &b.admin);
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");

        let post = |cookie: &str, extra: &str, ctype: &str| {
            let body = serde_json::json!({ "name": r.name }).to_string();
            b.send(&format!(
                "POST /admin/packet-logs/delete HTTP/1.1\r\nHost: {HOST}\r\nCookie: {cookie}\r\n{extra}Content-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ))
        };
        let (head, _) = post("", "", "application/json");
        assert!(head.starts_with("HTTP/1.1 401"), "{head}");
        let (head, _) = post(&b.viewer, "", "application/json");
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        let (head, _) = post(
            &b.admin,
            "Origin: https://evil.example\r\n",
            "application/json",
        );
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        let (head, _) = post(&b.admin, "Sec-Fetch-Site: cross-site\r\n", "text/plain");
        assert!(head.starts_with("HTTP/1.1 415"), "{head}");
        assert!(r.dir.join(&r.name).exists(), "nothing deleted");
        let (head, body) = post(
            &b.admin,
            &format!("Origin: http://{HOST}\r\n"),
            "application/json",
        );
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, br#"{"deleted":true}"#);
        assert!(!r.dir.join(&r.name).exists());
        b.listeners.stop();
    }

    /// Logging being off lowers nothing: the logs still hold voices.
    #[cfg(feature = "auth")]
    #[test]
    fn the_public_bind_refuses_stopped_logs_to_viewers_and_anonymous() {
        let r = stopped_log_rig();
        let b = public_bind(&r.ctrl);
        let file = format!("/admin/packet-logs/file?name={}", r.name);
        let (head, body) = b.get(&file, &b.admin);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, PCAP_BYTES);
        for path in ["/admin/packet-logs/state", file.as_str()] {
            let (head, _) = b.get(path, &b.admin);
            assert!(head.starts_with("HTTP/1.1 200"), "{path}: {head}");
            let (head, _) = b.get(path, &b.viewer);
            assert!(head.starts_with("HTTP/1.1 403"), "{path}: {head}");
            let (head, _) = b.get(path, "");
            assert!(head.starts_with("HTTP/1.1 401"), "{path}: {head}");
        }
        let post = |cookie: &str| {
            let body = serde_json::json!({ "name": r.name }).to_string();
            b.send(&format!(
                "POST /admin/packet-logs/delete HTTP/1.1\r\nHost: {HOST}\r\nCookie: {cookie}\r\nOrigin: http://{HOST}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ))
        };
        let (head, _) = post(&b.viewer);
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        let (head, _) = post("");
        assert!(head.starts_with("HTTP/1.1 401"), "{head}");
        assert!(r.dir.join(&r.name).exists(), "nothing deleted");
        let (head, _) = post(&b.admin);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(!r.dir.join(&r.name).exists());
        b.listeners.stop();
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
        let r = rig();
        let bind = public_bind(&r.ctrl);
        let (admin, viewer) = (bind.admin.clone(), bind.viewer.clone());
        let send = |raw: String| bind.send(&raw);
        let get = |path: &str, cookie: &str| bind.get(path, cookie);

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
        bind.listeners.stop();
    }
}
