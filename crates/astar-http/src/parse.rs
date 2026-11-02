// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! One request head, bounded. The rules are strict on purpose: one request
//! per connection and a refusal of anything ambiguous is what makes request
//! smuggling impossible here.

use std::io::{self, BufRead, Read as _};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
}

impl RequestHead {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    #[must_use]
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }
    #[must_use]
    pub fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, q)| q)
    }
}

#[derive(Debug)]
pub enum HeadError {
    TooLarge,
    Malformed(&'static str),
    CutShort,
    Io(io::Error),
}

impl HeadError {
    /// The status to answer with; `None`: close without a word (the peer
    /// went away, or the deadline passed).
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::TooLarge => Some(431),
            Self::Malformed(_) | Self::CutShort => Some(400),
            Self::Io(_) => None,
        }
    }
}

/// One line without its terminator: exactly one LF, then at most one CR.
/// Any other CR is left in place for [`check_controls`] to refuse.
fn strip_eol(text: &str) -> &str {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.strip_suffix('\r').unwrap_or(text)
}

/// Refuse control bytes (HTAB excepted) and DEL anywhere in a line.
fn check_controls(text: &str) -> Result<(), HeadError> {
    if text.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        return Err(HeadError::Malformed("control character"));
    }
    Ok(())
}

/// RFC 9110 `tchar`.
pub(crate) const fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

const SINGLE: &[&str] = &["host", "content-length", "transfer-encoding"];

/// Read one request head of at most `max_bytes`. `Ok(None)`: the connection
/// closed before a request line.
///
/// # Errors
/// [`HeadError`]; reads past `max_bytes` never happen (at most one byte more).
pub fn read_head<R: BufRead>(
    r: &mut R,
    max_bytes: usize,
) -> Result<Option<RequestHead>, HeadError> {
    let mut total = 0usize;
    let mut line = Vec::new();
    let mut next = |line: &mut Vec<u8>| -> Result<usize, HeadError> {
        line.clear();
        let budget = (max_bytes.saturating_sub(total)) as u64 + 1;
        let n = r
            .by_ref()
            .take(budget)
            .read_until(b'\n', line)
            .map_err(HeadError::Io)?;
        total += n;
        if total > max_bytes {
            return Err(HeadError::TooLarge);
        }
        Ok(n)
    };
    if next(&mut line)? == 0 {
        return Ok(None);
    }
    if !line.ends_with(b"\n") {
        return Err(HeadError::CutShort);
    }
    let text = std::str::from_utf8(&line).map_err(|_| HeadError::Malformed("not UTF-8"))?;
    let text = strip_eol(text);
    check_controls(text)?;
    let mut parts = text.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HeadError::Malformed("bad request line"));
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HeadError::Malformed("not HTTP/1.x"));
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(HeadError::Malformed("bad method"));
    }
    if !target.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(HeadError::Malformed("bad byte in target"));
    }
    if !(target.starts_with('/') || (target == "*" && method == "OPTIONS")) {
        return Err(HeadError::Malformed("not origin-form"));
    }
    let (method, target, version) = (method.to_string(), target.to_string(), version.to_string());
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let n = next(&mut line)?;
        if n == 0 || !line.ends_with(b"\n") {
            return Err(HeadError::CutShort);
        }
        let text = std::str::from_utf8(&line).map_err(|_| HeadError::Malformed("not UTF-8"))?;
        let text = strip_eol(text);
        check_controls(text)?;
        if text.is_empty() {
            break;
        }
        if text.starts_with([' ', '\t']) {
            return Err(HeadError::Malformed("obsolete line folding"));
        }
        let (name, value) = text
            .split_once(':')
            .ok_or(HeadError::Malformed("header without a colon"))?;
        if name.is_empty() || !name.bytes().all(is_tchar) {
            return Err(HeadError::Malformed("bad header name"));
        }
        if SINGLE.iter().any(|s| name.eq_ignore_ascii_case(s))
            && headers.iter().any(|(e, _)| e.eq_ignore_ascii_case(name))
        {
            return Err(HeadError::Malformed("repeated header"));
        }
        headers.push((name.to_string(), value.trim().to_string()));
    }
    Ok(Some(RequestHead {
        method,
        target,
        version,
        headers,
    }))
}
