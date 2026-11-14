// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Serving one open file with the byte-range rules a browser's media element
//! needs: [`send_file`] and the `Range` parser behind it.

use crate::{Conn, Response};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// How much of a file is copied at a time.
const CHUNK: usize = 64 * 1024;

/// One `Range` header against a file of `total` bytes, as the inclusive
/// span to send. `None`: ignore the header and send the whole file (not a
/// `bytes` range, malformed, `a > b`, or more than one range).
/// `Some(Err(()))`: unsatisfiable, answered `416`.
#[must_use]
pub fn parse_range(header: &str, total: u64) -> Option<Result<(u64, u64), ()>> {
    // The unit is case-insensitive (RFC 9110 section 14.1).
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

/// Answers the request with `file`, an open handle of `size` bytes, from
/// the position the handle is at (the start). `headers` go on the answer
/// as given (`Content-Type`, `Cache-Control`, `Content-Disposition`);
/// `Accept-Ranges: bytes` is always added. `range` is the request's `Range`
/// header: none, or one that is ignored, gets `200` and the whole file; one
/// satisfiable range gets `206` with `Content-Range`; one past the end gets
/// `416` with `Content-Range: bytes */size` and a JSON error. A `HEAD`
/// request gets the head alone. The span comes from `size` and the seek is
/// on `file` itself, so nothing is re-opened by path. A copy cut short
/// returns, which closes the connection short of `Content-Length` so the
/// client cannot take it for a whole answer. `name_for_log` is only for the
/// log line.
pub fn send_file(
    mut conn: Conn,
    mut file: File,
    size: u64,
    headers: &[(&str, &str)],
    range: Option<&str>,
    name_for_log: &str,
) {
    let (status, start, len, content_range) = match range.and_then(|h| parse_range(h, size)) {
        None => (200, 0, size, None),
        Some(Ok((first, last))) => (
            206,
            first,
            last - first + 1,
            Some(format!("bytes {first}-{last}/{size}")),
        ),
        Some(Err(())) => {
            let mut resp = Response::json(416, r#"{"error":"range not satisfiable"}"#)
                .header("Content-Range", format!("bytes */{size}"))
                .header("Accept-Ranges", "bytes");
            if let Some((_, v)) = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("cache-control"))
            {
                resp = resp.header("Cache-Control", *v);
            }
            conn.respond(&resp);
            return;
        }
    };
    let mut head: Vec<(&str, &str)> = headers.to_vec();
    head.push(("Accept-Ranges", "bytes"));
    if let Some(cr) = &content_range {
        head.push(("Content-Range", cr.as_str()));
    }
    if start > 0
        && let Err(e) = file.seek(SeekFrom::Start(start))
    {
        tracing::warn!("download: cannot seek {name_for_log:?}: {e}");
        conn.respond(
            &Response::json(500, r#"{"error":"cannot read the file"}"#)
                .header("Cache-Control", "no-store"),
        );
        return;
    }
    if conn.write_head_sized(status, &head, len).is_err() || conn.head_only() {
        return;
    }
    if let Err(e) = copy_exact(file, &mut conn, len) {
        // Returning closes the connection short of Content-Length.
        tracing::warn!("download: {name_for_log:?} cut short: {e}");
    }
}

/// Copies exactly `size` bytes; a source that runs dry first is an error.
fn copy_exact<R: Read, W: Write>(mut from: R, to: &mut W, size: u64) -> io::Result<()> {
    let mut left = size;
    let mut buf = vec![0u8; CHUNK];
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Limits, Reply, Server, StreamClass};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

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

    fn pattern() -> Vec<u8> {
        (0..2000u32).map(|i| (i % 251) as u8).collect()
    }

    /// One request; the head and the raw body.
    fn fetch(addr: std::net::SocketAddr, method: &str, extra: &str) -> (String, Vec<u8>) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            s,
            "{method} /f HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\n\r\n"
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

    #[test]
    fn send_file_answers_ranges_and_head() {
        let path = std::env::temp_dir().join(format!("astar-http-files-{}", std::process::id()));
        std::fs::write(&path, pattern()).unwrap();
        let served = path.clone();
        let stub = Server::bind("127.0.0.1:0", Limits::loopback(), move |req| {
            let range = req.header("range").map(str::to_owned);
            let served = served.clone();
            Reply::Stream {
                class: StreamClass::PerClientOnly,
                run: Box::new(move |conn| {
                    let file = std::fs::File::open(&served).unwrap();
                    let size = file.metadata().unwrap().len();
                    send_file(
                        conn,
                        file,
                        size,
                        &[("Content-Type", "application/octet-stream")],
                        range.as_deref(),
                        "test",
                    );
                }),
            }
        })
        .unwrap();
        let addr = stub.local_addr();
        let want = pattern();

        let (head, body) = fetch(addr, "GET", "Range: bytes=0-99\r\n");
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        assert!(
            head.contains("Content-Range: bytes 0-99/2000\r\n"),
            "{head}"
        );
        assert!(head.contains("Accept-Ranges: bytes\r\n"), "{head}");
        assert_eq!(body, &want[..100]);

        let (head, body) = fetch(addr, "GET", "Range: bytes=-10\r\n");
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        assert_eq!(body, &want[1990..]);

        let (head, body) = fetch(addr, "GET", "Range: bytes=5000-\r\n");
        assert!(head.starts_with("HTTP/1.1 416"), "{head}");
        assert!(head.contains("Content-Range: bytes */2000\r\n"), "{head}");
        assert_eq!(body, br#"{"error":"range not satisfiable"}"#);

        let (head, body) = fetch(addr, "GET", "");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Accept-Ranges: bytes\r\n"), "{head}");
        assert_eq!(body, want);

        let (head, body) = fetch(addr, "HEAD", "");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Content-Length: 2000\r\n"), "{head}");
        assert_eq!(body.len(), 0);

        stub.stop();
        let _ = std::fs::remove_file(&path);
    }
}
