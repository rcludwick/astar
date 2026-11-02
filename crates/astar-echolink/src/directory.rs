// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Directory server lines (login, status, logoff), its replies, and the station list.
//!
//! The directory speaks plain text over TCP. A station logs in by sending one
//! line made of its callsign, password, status and description, and the server
//! answers with a short reply. Asking for the station list is the single byte
//! [`LIST_REQUEST`]; the answer is either a plain `@@@` list or the same list
//! compressed with zlib. Nothing here touches a socket: the caller moves bytes.

use std::net::Ipv4Addr;

use zeroize::Zeroizing;

/// Separates the callsign from the password in a login line.
const CALL_PASSWORD_SEPARATOR: &[u8] = &[0xAC, 0xAC];
/// Ends each field of a directory line.
const CR: u8 = b'\r';

/// Asks the directory for the full station list.
pub const LIST_REQUEST: &[u8] = b"s";

/// Largest reply accepted off the wire, compressed or not.
pub const MAX_RAW_LIST_BYTES: usize = 4 * 1024 * 1024;
/// Largest list accepted after inflating a zlib reply.
pub const MAX_INFLATED_LIST_BYTES: usize = 16 * 1024 * 1024;
/// Longest text kept from a login refusal.
pub const MAX_REFUSAL_CHARS: usize = 80;
/// Without a line end or EOF, a reply this long is judged as it stands.
const LOGIN_REPLY_LOOKAHEAD: usize = 256;

/// Most entries accepted in one list, counting the ones that are skipped.
pub const MAX_LIST_ENTRIES: usize = 100_000;

const LIST_START: &[u8] = b"@@@";
const DELTA_START: &[u8] = b"DDD";
const LIST_END: &str = "+++";
const MESSAGE_CALL: &str = ".";

/// The status a logged-in station reports to the directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Online,
    Busy,
}

/// The line that logs in, or refreshes the status of, a station.
///
/// The password is written straight into a buffer that is zeroed when dropped;
/// it is never formatted through `format!` or kept anywhere else.
#[must_use]
pub fn login_line(
    callsign: &str,
    password: &str,
    status: Status,
    hhmm: (u8, u8),
    description: &str,
) -> Zeroizing<Vec<u8>> {
    let word = match status {
        Status::Online => "ONLINE3.38",
        Status::Busy => "BUSY3.40",
    };
    let state = format!("{word}({:02}:{:02})", hhmm.0, hhmm.1);
    directory_line(callsign, password, &state, description)
}

/// The line that logs a station off.
#[must_use]
pub fn logoff_line(callsign: &str, password: &str, description: &str) -> Zeroizing<Vec<u8>> {
    directory_line(callsign, password, "OFF-V3.40", description)
}

/// `l` + callsign + `AC AC` + password + CR + state + CR + description + CR.
///
/// CR and LF in the callsign or description become spaces, so neither can
/// start a line of its own.
fn directory_line(
    callsign: &str,
    password: &str,
    state: &str,
    description: &str,
) -> Zeroizing<Vec<u8>> {
    let len = 1
        + callsign.len()
        + CALL_PASSWORD_SEPARATOR.len()
        + password.len()
        + 1
        + state.len()
        + 1
        + description.len()
        + 1;
    let mut out = Zeroizing::new(Vec::with_capacity(len));
    out.push(b'l');
    push_single_line(&mut out, callsign);
    out.extend_from_slice(CALL_PASSWORD_SEPARATOR);
    out.extend_from_slice(password.as_bytes());
    out.push(CR);
    out.extend_from_slice(state.as_bytes());
    out.push(CR);
    push_single_line(&mut out, description);
    out.push(CR);
    debug_assert_eq!(
        out.len(),
        len,
        "capacity was exact, so the buffer never moved"
    );
    out
}

fn push_single_line(out: &mut Vec<u8>, text: &str) {
    out.extend(
        text.bytes()
            .map(|b| if b == b'\r' || b == b'\n' { b' ' } else { b }),
    );
}

/// What the directory said to a login line.
///
/// It holds only the server's own words, never anything the station sent, so
/// its `Debug` is safe to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginReply {
    Ok,
    BadPassword,
    /// Any other reply: its first line, printable ASCII only, at most
    /// [`MAX_REFUSAL_CHARS`] characters.
    Refused(String),
}

/// Judge the bytes received so far after a login line.
///
/// Line ends before the first text are skipped.
/// Returns `None` while the reply could still turn out either way: no line end
/// yet, no EOF, and not long enough to judge as it stands.
#[must_use]
pub fn parse_login_reply(bytes: &[u8], eof: bool) -> Option<LoginReply> {
    if bytes.starts_with(b"OK") {
        return Some(LoginReply::Ok);
    }
    if bytes.starts_with(b"INCORRECT PASSWORD") {
        return Some(LoginReply::BadPassword);
    }
    let start = bytes
        .iter()
        .position(|&b| b != b'\r' && b != b'\n')
        .unwrap_or(bytes.len());
    let body = &bytes[start..];
    let line_end = body.iter().position(|&b| b == b'\r' || b == b'\n');
    if line_end.is_none() && !eof && bytes.len() < LOGIN_REPLY_LOOKAHEAD {
        return None;
    }
    let first = &body[..line_end.unwrap_or(body.len())];
    let text: String = first
        .iter()
        .filter(|b| (0x20..0x7f).contains(*b))
        .take(MAX_REFUSAL_CHARS)
        .map(|&b| char::from(b))
        .collect();
    Some(LoginReply::Refused(text.trim().to_owned()))
}

/// What kind of station an entry is, read from the shape of its callsign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StationKind {
    User,
    /// Callsign ends in `-L`: a simplex RF link.
    Link,
    /// Callsign ends in `-R`: a repeater.
    Repeater,
    /// Callsign is wrapped in stars, like `*ECHOTEST*`.
    Conference,
}

/// Whether a listed station is free or in a contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StationStatus {
    Online,
    Busy,
    /// The description carried no `[ON hh:mm]` / `[BUSY hh:mm]` tag.
    Unknown,
}

/// One station from the directory list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationEntry {
    /// Uppercased.
    pub call: String,
    /// With the status tag removed.
    pub description: String,
    pub node: u32,
    pub ip: Ipv4Addr,
    pub kind: StationKind,
    pub status: StationStatus,
}

/// A parsed directory list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StationList {
    pub stations: Vec<StationEntry>,
    /// Text the directory itself puts in the list, under the callsign `.`.
    pub messages: Vec<String>,
}

/// Why a list reply was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ListError {
    #[error("the station list is too large")]
    TooLarge,
    #[error("the directory sent a list update that was never asked for")]
    DeltaNotRequested,
    #[error("the compressed station list could not be unpacked")]
    Inflate,
    #[error("the station list is malformed: {0}")]
    Malformed(&'static str),
}

/// Collects a list reply as it arrives, then parses it at EOF.
///
/// The reply may arrive split at any byte, so `push` only gathers bytes and
/// refuses early what can be refused from the bytes alone (too large, a `DDD`
/// delta). `finish` does the parsing.
#[derive(Debug, Default)]
pub struct ListParser {
    buf: Vec<u8>,
    failed: Option<ListError>,
}

impl ListParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the next bytes of the reply.
    ///
    /// # Errors
    /// [`ListError::TooLarge`] past [`MAX_RAW_LIST_BYTES`];
    /// [`ListError::DeltaNotRequested`] once the reply is seen to start `DDD`.
    /// After an error every later call returns the same error.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ListError> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if self.buf.len() + bytes.len() > MAX_RAW_LIST_BYTES {
            return Err(*self.failed.insert(ListError::TooLarge));
        }
        self.buf.extend_from_slice(bytes);
        if self.buf.starts_with(DELTA_START) {
            return Err(*self.failed.insert(ListError::DeltaNotRequested));
        }
        Ok(())
    }

    /// Parse everything received, called once the directory closes the connection.
    ///
    /// # Errors
    /// Any [`ListError`]: the reply is too large, a delta, not valid zlib, or
    /// not shaped like a list.
    pub fn finish(self) -> Result<StationList, ListError> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if self.buf.is_empty() {
            return Err(ListError::Malformed("the directory sent nothing"));
        }
        if self.buf.starts_with(LIST_START) {
            return parse_list(&self.buf);
        }
        let inflated = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(
            &self.buf,
            MAX_INFLATED_LIST_BYTES,
        )
        .map_err(|e| {
            if e.status == miniz_oxide::inflate::TINFLStatus::HasMoreOutput {
                ListError::TooLarge
            } else {
                ListError::Inflate
            }
        })?;
        if inflated.starts_with(DELTA_START) {
            return Err(ListError::DeltaNotRequested);
        }
        if !inflated.starts_with(LIST_START) {
            return Err(ListError::Malformed(
                "the unpacked reply is not a station list",
            ));
        }
        parse_list(&inflated)
    }
}

/// Parse a plain list: `@@@`, a count, four lines per entry, then `+++`.
///
/// The count is not trusted: entries are read until `+++`. An entry whose node
/// number or IP address does not parse is skipped. Lines are decoded one at a
/// time as they are read, and more than [`MAX_LIST_ENTRIES`] entries (kept or
/// skipped) is [`ListError::TooLarge`], so a body of blank lines costs no more
/// memory than the body itself.
fn parse_list(bytes: &[u8]) -> Result<StationList, ListError> {
    let mut lines = Lines { rest: bytes }.map(decode);
    if lines.next().as_deref().map(str::trim) != Some("@@@") {
        return Err(ListError::Malformed("the list does not start with @@@"));
    }
    if lines.next().is_none() {
        return Err(ListError::Malformed("the list has no count line"));
    }
    let mut list = StationList::default();
    let mut entries = 0usize;
    loop {
        let Some(call) = lines.next() else {
            return Err(ListError::Malformed("the list ended before +++"));
        };
        if call.trim() == LIST_END {
            return Ok(list);
        }
        entries += 1;
        if entries > MAX_LIST_ENTRIES {
            return Err(ListError::TooLarge);
        }
        let mut field = || match lines.next() {
            Some(l) if l.trim() == LIST_END => Err(ListError::Malformed("an entry was cut short")),
            Some(l) => Ok(l),
            None => Err(ListError::Malformed("the list ended before +++")),
        };
        let (description, node, ip) = (field()?, field()?, field()?);
        let call = call.trim();
        if call == MESSAGE_CALL {
            let message = description.trim();
            if !message.is_empty() {
                list.messages.push(message.to_owned());
            }
            continue;
        }
        let (Ok(node), Ok(ip)) = (node.trim().parse::<u32>(), ip.trim().parse::<Ipv4Addr>()) else {
            continue;
        };
        if call.is_empty() {
            continue;
        }
        let call = call.to_uppercase();
        let (description, status) = split_status(description.trim());
        list.stations.push(StationEntry {
            kind: kind_of(&call),
            call,
            description: description.to_owned(),
            node,
            ip,
            status,
        });
    }
}

/// The lines of a body, split at `\n`, `\r\n` or a lone `\r`, without copying.
struct Lines<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Lines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let Some(end) = self.rest.iter().position(|&b| b == b'\r' || b == b'\n') else {
            let line = self.rest;
            self.rest = &[];
            return Some(line);
        };
        let line = &self.rest[..end];
        let skip = if self.rest[end] == b'\r' && self.rest.get(end + 1) == Some(&b'\n') {
            2
        } else {
            1
        };
        self.rest = &self.rest[end + skip..];
        Some(line)
    }
}

/// Decode one line as text: UTF-8 when it is valid, Latin-1 when it is not,
/// with control characters dropped.
fn decode(line: &[u8]) -> String {
    let text = match std::str::from_utf8(line) {
        Ok(s) => s.to_owned(),
        Err(_) => line.iter().map(|&b| char::from(b)).collect(),
    };
    text.chars().filter(|c| !c.is_control()).collect()
}

pub(crate) fn kind_of(call: &str) -> StationKind {
    if call.len() >= 2 && call.starts_with('*') && call.ends_with('*') {
        StationKind::Conference
    } else if call.ends_with("-L") {
        StationKind::Link
    } else if call.ends_with("-R") {
        StationKind::Repeater
    } else {
        StationKind::User
    }
}

/// Take a trailing `[ON hh:mm]` or `[BUSY hh:mm]` off a description.
///
/// Any other trailing bracket is left in place and the status is unknown.
fn split_status(description: &str) -> (&str, StationStatus) {
    let unknown = (description, StationStatus::Unknown);
    let Some(inner) = description.strip_suffix(']') else {
        return unknown;
    };
    let Some(open) = inner.rfind('[') else {
        return unknown;
    };
    let Some((word, time)) = inner[open + 1..].split_once(' ') else {
        return unknown;
    };
    let status = match word {
        "ON" => StationStatus::Online,
        "BUSY" => StationStatus::Busy,
        _ => return unknown,
    };
    let t = time.as_bytes();
    let is_hhmm =
        t.len() == 5 && t[2] == b':' && [0, 1, 3, 4].iter().all(|&k| t[k].is_ascii_digit());
    if !is_hhmm {
        return unknown;
    }
    (description[..open].trim_end(), status)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &[u8] = b"@@@\n4\n.\nWelcome to the directory\n0\n0.0.0.0\n*ECHOTEST*\nTest server [ON 12:00]\n9999\n127.0.0.1\nKX7ABC-L\nDenver [BUSY 08:30]\n123456\n127.0.0.2\nW1AW\nNewington\n1234\n127.0.0.3\n+++";

    fn parse_all(bytes: &[u8]) -> Result<StationList, ListError> {
        let mut p = ListParser::new();
        p.push(bytes)?;
        p.finish()
    }

    fn entry(
        call: &str,
        description: &str,
        node: u32,
        ip: [u8; 4],
        kind: StationKind,
        status: StationStatus,
    ) -> StationEntry {
        StationEntry {
            call: call.to_owned(),
            description: description.to_owned(),
            node,
            ip: Ipv4Addr::from(ip),
            kind,
            status,
        }
    }

    fn expected() -> StationList {
        StationList {
            stations: vec![
                entry(
                    "*ECHOTEST*",
                    "Test server",
                    9999,
                    [127, 0, 0, 1],
                    StationKind::Conference,
                    StationStatus::Online,
                ),
                entry(
                    "KX7ABC-L",
                    "Denver",
                    123_456,
                    [127, 0, 0, 2],
                    StationKind::Link,
                    StationStatus::Busy,
                ),
                entry(
                    "W1AW",
                    "Newington",
                    1234,
                    [127, 0, 0, 3],
                    StationKind::User,
                    StationStatus::Unknown,
                ),
            ],
            messages: vec!["Welcome to the directory".to_owned()],
        }
    }

    #[test]
    fn login_line_golden() {
        assert_eq!(
            &*login_line("KX7ABC", "pw", Status::Busy, (13, 5), "Denver"),
            b"lKX7ABC\xAC\xACpw\rBUSY3.40(13:05)\rDenver\r"
        );
        assert_eq!(
            &*login_line("KX7ABC", "pw", Status::Online, (9, 0), "Denver"),
            b"lKX7ABC\xAC\xACpw\rONLINE3.38(09:00)\rDenver\r"
        );
    }

    #[test]
    fn logoff_line_golden() {
        assert_eq!(
            &*logoff_line("KX7ABC", "pw", "Denver"),
            b"lKX7ABC\xAC\xACpw\rOFF-V3.40\rDenver\r"
        );
    }

    #[test]
    fn line_breaks_in_the_description_cannot_add_lines() {
        let line = login_line("KX7ABC", "pw", Status::Busy, (1, 2), "a\rb\nc");
        assert_eq!(&*line, b"lKX7ABC\xAC\xACpw\rBUSY3.40(01:02)\ra b c\r");
    }

    #[test]
    fn login_reply_ok_bad_and_refused() {
        assert_eq!(parse_login_reply(b"OK", false), Some(LoginReply::Ok));
        assert_eq!(parse_login_reply(b"OK\r", true), Some(LoginReply::Ok));
        assert_eq!(
            parse_login_reply(b"INCORRECT PASSWORD\r", false),
            Some(LoginReply::BadPassword)
        );
        assert_eq!(
            parse_login_reply(b"NOPE\r", true),
            Some(LoginReply::Refused("NOPE".to_owned()))
        );
        assert_eq!(
            parse_login_reply(b"NOPE\r", false),
            Some(LoginReply::Refused("NOPE".to_owned()))
        );
        assert_eq!(parse_login_reply(b"O", false), None);
        assert_eq!(parse_login_reply(b"INCORRECT PASS", false), None);
        assert_eq!(parse_login_reply(b"NOP", false), None);
        assert_eq!(parse_login_reply(b"", false), None);
        assert_eq!(
            parse_login_reply(b"", true),
            Some(LoginReply::Refused(String::new()))
        );

        let mut long = Vec::new();
        for i in 0..200u8 {
            long.push(if i % 7 == 0 { 0x01 } else { b'A' + i % 26 });
        }
        for eof in [true, false] {
            let mut bytes = long.clone();
            if !eof {
                bytes.push(b'\r');
            }
            match parse_login_reply(&bytes, eof) {
                Some(LoginReply::Refused(text)) => {
                    assert!(text.len() <= 80, "{} bytes", text.len());
                    assert!(text.bytes().all(|b| (0x20..0x7f).contains(&b)), "{text:?}");
                    assert_ne!(text, "");
                }
                other => panic!("expected Refused, got {other:?}"),
            }
        }
        // Only the first line is kept.
        assert_eq!(
            parse_login_reply(b"NOT VALIDATED\r\nsecond line\r", true),
            Some(LoginReply::Refused("NOT VALIDATED".to_owned()))
        );
    }

    #[test]
    fn refusal_skips_leading_line_ends() {
        assert_eq!(
            parse_login_reply(b"\r\nNOT VALIDATED", true),
            Some(LoginReply::Refused("NOT VALIDATED".to_owned()))
        );
        assert_eq!(
            parse_login_reply(b"\n\rNOT VALIDATED\r", false),
            Some(LoginReply::Refused("NOT VALIDATED".to_owned()))
        );
        // Line ends alone are not yet a reply.
        assert_eq!(parse_login_reply(b"\r\n", false), None);
        assert_eq!(
            parse_login_reply(b"\r\n", true),
            Some(LoginReply::Refused(String::new()))
        );
    }

    #[test]
    fn a_body_of_blank_lines_is_refused_quickly() {
        let mut body = b"@@@\n1\n".to_vec();
        body.resize(MAX_INFLATED_LIST_BYTES, b'\n');
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&body, 6);
        assert!(packed.len() < MAX_RAW_LIST_BYTES);
        let started = std::time::Instant::now();
        let got = parse_all(&packed);
        assert_eq!(got, Err(ListError::TooLarge));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn more_than_the_entry_cap_is_too_large() {
        let list_of = |n: usize| {
            use std::fmt::Write as _;
            let mut text = format!("@@@\n{n}\n");
            for i in 0..n {
                let _ = write!(text, "W{i}\nx\n{i}\n127.0.0.1\n");
            }
            text.push_str("+++\n");
            text.into_bytes()
        };
        let at_cap = list_of(MAX_LIST_ENTRIES);
        assert!(at_cap.len() < MAX_RAW_LIST_BYTES);
        assert_eq!(parse_all(&at_cap).unwrap().stations.len(), MAX_LIST_ENTRIES);
        assert_eq!(
            parse_all(&list_of(MAX_LIST_ENTRIES + 1)),
            Err(ListError::TooLarge)
        );
    }

    #[test]
    fn uncompressed_list_parses_kinds_status_and_messages() {
        assert_eq!(parse_all(LIST).unwrap(), expected());
    }

    #[test]
    fn list_parses_identically_at_every_split_point() {
        let whole = parse_all(LIST).unwrap();
        for k in 0..LIST.len() {
            let mut p = ListParser::new();
            p.push(&LIST[..k]).unwrap();
            p.push(&LIST[k..]).unwrap();
            assert_eq!(p.finish().unwrap(), whole, "split at {k}");
        }
    }

    #[test]
    fn list_parses_byte_by_byte() {
        let mut p = ListParser::new();
        for b in LIST {
            p.push(std::slice::from_ref(b)).unwrap();
        }
        assert_eq!(p.finish().unwrap(), expected());
    }

    #[test]
    fn list_accepts_crlf_line_ends() {
        let crlf: Vec<u8> = String::from_utf8(LIST.to_vec())
            .unwrap()
            .replace('\n', "\r\n")
            .into_bytes();
        assert_eq!(parse_all(&crlf).unwrap(), expected());
        let cr: Vec<u8> = String::from_utf8(LIST.to_vec())
            .unwrap()
            .replace('\n', "\r")
            .into_bytes();
        assert_eq!(parse_all(&cr).unwrap(), expected());
        // A CRLF split across two pushes still counts as one line end.
        for k in 0..crlf.len() {
            let mut p = ListParser::new();
            p.push(&crlf[..k]).unwrap();
            p.push(&crlf[k..]).unwrap();
            assert_eq!(p.finish().unwrap(), expected(), "split at {k}");
        }
    }

    #[test]
    fn count_line_disagreeing_with_entries_keeps_the_entries() {
        for count in ["0", "1", "99", "x"] {
            let text = String::from_utf8(LIST.to_vec()).unwrap().replacen(
                "@@@\n4\n",
                &format!("@@@\n{count}\n"),
                1,
            );
            assert_eq!(
                parse_all(text.as_bytes()).unwrap(),
                expected(),
                "count {count}"
            );
        }
    }

    #[test]
    fn zlib_reply_is_inflated_then_parsed() {
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(LIST, 6);
        assert_eq!(parse_all(&packed).unwrap(), expected());
        for k in 0..packed.len() {
            let mut p = ListParser::new();
            p.push(&packed[..k]).unwrap();
            p.push(&packed[k..]).unwrap();
            assert_eq!(p.finish().unwrap(), expected(), "split at {k}");
        }
    }

    #[test]
    fn garbage_that_is_not_zlib_is_an_inflate_error() {
        assert_eq!(parse_all(b"hello there\n"), Err(ListError::Inflate));
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(LIST, 6);
        assert_eq!(
            parse_all(&packed[..packed.len() / 2]),
            Err(ListError::Inflate)
        );
    }

    #[test]
    fn ddd_reply_is_delta_not_requested() {
        assert_eq!(
            parse_all(b"DDD\n12\nwhatever"),
            Err(ListError::DeltaNotRequested)
        );
        let mut p = ListParser::new();
        p.push(b"DD").unwrap();
        assert_eq!(p.push(b"D\n"), Err(ListError::DeltaNotRequested));
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(b"DDD\n1\n", 6);
        assert_eq!(parse_all(&packed), Err(ListError::DeltaNotRequested));
    }

    #[test]
    fn oversize_reply_is_too_large() {
        let mut p = ListParser::new();
        let chunk = vec![b'@'; 1024 * 1024];
        for _ in 0..4 {
            p.push(&chunk).unwrap();
        }
        assert_eq!(p.push(b"x"), Err(ListError::TooLarge));

        let mut bomb = b"@@@\n1\n".to_vec();
        bomb.resize(17 * 1024 * 1024, b'a');
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&bomb, 6);
        assert!(packed.len() < 4 * 1024 * 1024);
        assert_eq!(parse_all(&packed), Err(ListError::TooLarge));
    }

    #[test]
    fn repeater_suffix_and_lowercase_calls_classify() {
        let list = b"@@@\n3\nkx7abc-r\nMountain top\n55\n127.0.0.9\n*conf*\nRound table\n56\n127.0.0.10\nw1aw-l\n\n57\n127.0.0.11\n+++\n";
        let got = parse_all(list).unwrap();
        assert_eq!(
            got.stations,
            vec![
                entry(
                    "KX7ABC-R",
                    "Mountain top",
                    55,
                    [127, 0, 0, 9],
                    StationKind::Repeater,
                    StationStatus::Unknown
                ),
                entry(
                    "*CONF*",
                    "Round table",
                    56,
                    [127, 0, 0, 10],
                    StationKind::Conference,
                    StationStatus::Unknown
                ),
                entry(
                    "W1AW-L",
                    "",
                    57,
                    [127, 0, 0, 11],
                    StationKind::Link,
                    StationStatus::Unknown
                ),
            ]
        );
    }

    #[test]
    fn status_bracket_is_only_stripped_when_it_is_on_or_busy() {
        let list = b"@@@\n4\nA1A\nNet [OFF 12:00]\n1\n127.0.0.1\nB1B\n[ON 1:00]\n2\n127.0.0.2\nC1C\n  Club  [BUSY 23:59]  \n3\n127.0.0.3\nD1D\n[ON 07:15]\n4\n127.0.0.4\n+++";
        let got = parse_all(list).unwrap().stations;
        let summary: Vec<(&str, StationStatus)> = got
            .iter()
            .map(|s| (s.description.as_str(), s.status))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Net [OFF 12:00]", StationStatus::Unknown),
                ("[ON 1:00]", StationStatus::Unknown),
                ("Club", StationStatus::Busy),
                ("", StationStatus::Online),
            ]
        );
    }

    #[test]
    fn entries_with_a_bad_node_or_ip_are_skipped() {
        let list = b"@@@\n4\nA1A\nx\nnotanumber\n127.0.0.1\nB1B\ny\n2\n999.0.0.1\nC1C\nz\n-3\n127.0.0.3\nW1AW\nNewington\n1234\n127.0.0.3\n+++";
        let got = parse_all(list).unwrap();
        assert_eq!(
            got.stations,
            vec![entry(
                "W1AW",
                "Newington",
                1234,
                [127, 0, 0, 3],
                StationKind::User,
                StationStatus::Unknown
            )]
        );
    }

    #[test]
    fn truncated_or_shapeless_lists_are_refused_without_panicking() {
        let cut = &LIST[..LIST.len() - 3];
        assert!(matches!(parse_all(cut), Err(ListError::Malformed(_))));
        assert!(matches!(parse_all(b""), Err(ListError::Malformed(_))));
        assert!(matches!(parse_all(b"@@@"), Err(ListError::Malformed(_))));
        assert!(matches!(
            parse_all(b"@@@\n1\nA1A\nonly two\n+++"),
            Err(ListError::Malformed(_))
        ));
        // Everything after the terminator is ignored.
        let mut trailing = LIST.to_vec();
        trailing.extend_from_slice(b"\r\nnoise\n");
        assert_eq!(parse_all(&trailing).unwrap(), expected());
        // Arbitrary bytes after the marker never panic.
        for seed in 0u32..200 {
            let mut bytes = b"@@@\n".to_vec();
            let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
            for _ in 0..(seed % 97) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                bytes.push(x.to_le_bytes()[0]);
            }
            let _ = parse_all(&bytes);
        }
    }
}
