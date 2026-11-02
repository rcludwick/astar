// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A minimal Asterisk Manager Interface client: TCP, `Login`, `Command`,
//! arbitrary actions, and event reading.
//!
//! Messages are `Key: Value` lines ended by a blank line. Responses are
//! matched to actions by `ActionID`; events that arrive while waiting are
//! queued for [`AmiClient::next_event`]. Both shapes of `Command` output are
//! understood: `Output:` lines (Asterisk 14+), and the legacy
//! `Response: Follows` ... `--END COMMAND--` block.
//!
//! The login secret is used once and never stored, logged, or put in an
//! error.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// One AMI message: ordered `Key: Value` pairs, plus `Command` output lines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AmiMessage {
    pub fields: Vec<(String, String)>,
    /// `Output:` lines (or the legacy `Follows` body), in order.
    pub output: Vec<String>,
}

impl AmiMessage {
    /// First value for `key` (case-insensitive, as Asterisk treats keys).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    #[must_use]
    pub fn is_event(&self) -> bool {
        self.get("Event").is_some()
    }

    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self.get("Response"), Some(r) if r.eq_ignore_ascii_case("Success") || r.eq_ignore_ascii_case("Follows"))
    }

    /// The command output joined back into CLI text.
    #[must_use]
    pub fn output_text(&self) -> String {
        let mut s = self.output.join("\n");
        if !s.is_empty() {
            s.push('\n');
        }
        s
    }
}

/// AMI failures. Never carries the login secret.
#[derive(Debug)]
pub enum AmiError {
    Io(io::Error),
    /// The far end did not greet as an Asterisk Call Manager.
    Banner(String),
    /// `Response: Error` — the `Message:` text.
    Refused(String),
    /// The connection closed mid-message.
    Closed,
    Timeout,
}

impl fmt::Display for AmiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "AMI i/o: {e}"),
            Self::Banner(b) => write!(f, "AMI: unexpected banner {b:?}"),
            Self::Refused(m) => write!(f, "AMI refused: {m}"),
            Self::Closed => write!(f, "AMI connection closed"),
            Self::Timeout => write!(f, "AMI timed out"),
        }
    }
}

impl std::error::Error for AmiError {}

impl From<io::Error> for AmiError {
    fn from(e: io::Error) -> Self {
        if matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            Self::Timeout
        } else {
            Self::Io(e)
        }
    }
}

/// Read one message from `r`. `Ok(None)` is a clean EOF between messages.
pub fn read_message(r: &mut impl BufRead) -> Result<Option<AmiMessage>, AmiError> {
    let mut msg = AmiMessage::default();
    let mut line = String::new();
    let mut follows = false;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return if msg.fields.is_empty() && msg.output.is_empty() {
                Ok(None)
            } else {
                Err(AmiError::Closed)
            };
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if follows {
            if let Some(rest) = l.strip_suffix("--END COMMAND--") {
                // The last output line can share a line with the marker.
                if !rest.is_empty() {
                    msg.output.extend(rest.lines().map(str::to_string));
                }
                follows = false;
                continue;
            }
            // The legacy block still carries Privilege/ActionID headers
            // before the raw output starts.
            if msg.output.is_empty()
                && let Some((k, v)) = l.split_once(": ")
                && (k.eq_ignore_ascii_case("Privilege") || k.eq_ignore_ascii_case("ActionID"))
            {
                msg.fields.push((k.to_string(), v.to_string()));
                continue;
            }
            msg.output.push(l.to_string());
            continue;
        }
        if l.is_empty() {
            if msg.fields.is_empty() && msg.output.is_empty() {
                continue; // stray blank line between messages
            }
            return Ok(Some(msg));
        }
        match l.split_once(':') {
            Some((k, v)) => {
                let v = v.strip_prefix(' ').unwrap_or(v);
                if k.eq_ignore_ascii_case("Output") {
                    msg.output.push(v.to_string());
                } else {
                    if k.eq_ignore_ascii_case("Response") && v.eq_ignore_ascii_case("Follows") {
                        follows = true;
                    }
                    msg.fields.push((k.to_string(), v.to_string()));
                }
            }
            // A bare line inside a response: treat as output.
            None => msg.output.push(l.to_string()),
        }
    }
}

/// A connected AMI session.
pub struct AmiClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
    events: VecDeque<AmiMessage>,
    /// The `Asterisk Call Manager/x.y.z` banner.
    pub banner: String,
}

impl fmt::Debug for AmiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AmiClient")
            .field("banner", &self.banner)
            .field("queued_events", &self.events.len())
            .finish_non_exhaustive()
    }
}

impl AmiClient {
    /// Connect and read the banner. `timeout` bounds connect and each read.
    pub fn connect(addr: SocketAddr, timeout: Duration) -> Result<Self, AmiError> {
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        let mut banner = String::new();
        reader.read_line(&mut banner)?;
        let banner = banner.trim_end().to_string();
        if !banner.starts_with("Asterisk Call Manager") {
            return Err(AmiError::Banner(banner));
        }
        Ok(Self {
            reader,
            writer,
            next_id: 1,
            events: VecDeque::new(),
            banner,
        })
    }

    /// `Action: Login`. `events` turns the event stream on (`Events: on`).
    pub fn login(&mut self, user: &str, secret: &str, events: bool) -> Result<(), AmiError> {
        let ev = if events { "on" } else { "off" };
        let r = self.action(
            "Login",
            &[("Username", user), ("Secret", secret), ("Events", ev)],
        )?;
        if r.is_success() {
            Ok(())
        } else {
            Err(AmiError::Refused(
                r.get("Message").unwrap_or("login failed").to_string(),
            ))
        }
    }

    /// Send any action and wait for its response (matched by `ActionID`).
    pub fn action(&mut self, name: &str, args: &[(&str, &str)]) -> Result<AmiMessage, AmiError> {
        let id = format!("nb-{}", self.next_id);
        self.next_id += 1;
        let mut req = format!("Action: {name}\r\nActionID: {id}\r\n");
        for (k, v) in args {
            // Values are single-line by protocol; refuse to smuggle a newline.
            let v = v.replace(['\r', '\n'], " ");
            let _ = write!(req, "{k}: {v}\r\n");
        }
        req.push_str("\r\n");
        self.writer.write_all(req.as_bytes())?;
        self.writer.flush()?;
        loop {
            let msg = read_message(&mut self.reader)?.ok_or(AmiError::Closed)?;
            if msg.is_event() {
                self.events.push_back(msg);
                continue;
            }
            if msg.get("ActionID") == Some(id.as_str()) || msg.get("ActionID").is_none() {
                return Ok(msg);
            }
            // A response to some other action (should not happen; one
            // in-flight action at a time) — drop it.
        }
    }

    /// `Action: Command` — run a CLI command and return its text.
    pub fn command(&mut self, cli: &str) -> Result<String, AmiError> {
        let r = self.action("Command", &[("Command", cli)])?;
        if r.is_success() {
            Ok(r.output_text())
        } else {
            Err(AmiError::Refused(
                r.get("Message")
                    .or_else(|| r.output.first().map(String::as_str))
                    .unwrap_or("command failed")
                    .to_string(),
            ))
        }
    }

    /// The next event, waiting up to `timeout`. `Ok(None)` on timeout.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<AmiMessage>, AmiError> {
        if let Some(e) = self.events.pop_front() {
            return Ok(Some(e));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.reader.get_ref().set_read_timeout(Some(left))?;
            match read_message(&mut self.reader) {
                Ok(Some(m)) if m.is_event() => return Ok(Some(m)),
                Ok(Some(_)) => {}
                Ok(None) => return Err(AmiError::Closed),
                Err(AmiError::Timeout) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// `Action: Logoff`, best effort.
    pub fn logoff(mut self) {
        let _ = self.action("Logoff", &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};
    use std::net::TcpListener;

    #[test]
    fn parses_modern_command_response() {
        let raw = "Response: Success\r\nActionID: nb-2\r\nMessage: Command output follows\r\nOutput: \r\nOutput: ***** CONNECTED NODES *****\r\nOutput: T1998\r\n\r\n";
        let m = read_message(&mut Cursor::new(raw)).unwrap().unwrap();
        assert!(m.is_success());
        assert_eq!(m.get("actionid"), Some("nb-2"));
        assert_eq!(m.output, vec!["", "***** CONNECTED NODES *****", "T1998"]);
    }

    #[test]
    fn parses_legacy_follows_response() {
        let raw = "Response: Follows\r\nPrivilege: Command\r\nActionID: nb-3\r\nline one\nline: two\n--END COMMAND--\r\n\r\n";
        let m = read_message(&mut Cursor::new(raw)).unwrap().unwrap();
        assert!(m.is_success());
        assert_eq!(m.output, vec!["line one", "line: two"]);
    }

    #[test]
    fn parses_event_and_eof() {
        let raw = "\r\nEvent: RPT_LINKS\r\nNode: 1999\r\nEventValue: 1,T1998\r\n\r\n";
        let mut c = Cursor::new(raw);
        let m = read_message(&mut c).unwrap().unwrap();
        assert!(m.is_event());
        assert_eq!(m.get("EventValue"), Some("1,T1998"));
        assert!(read_message(&mut c).unwrap().is_none());
    }

    #[test]
    fn truncated_message_is_closed() {
        let raw = "Response: Success\r\nActionID: x\r\n";
        assert!(matches!(
            read_message(&mut Cursor::new(raw)),
            Err(AmiError::Closed)
        ));
    }

    /// A scripted AMI peer on 127.0.0.1: login, one command (with an event
    /// interleaved before its response), then an event.
    #[test]
    fn client_round_trip_against_fake_ami() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(b"Asterisk Call Manager/9.0.0\r\n").unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let read_req = |r: &mut BufReader<TcpStream>| {
                let mut req = String::new();
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    req.push_str(&line);
                }
                req
            };
            let login = read_req(&mut r);
            assert!(login.contains("Action: Login"));
            assert!(login.contains("Username: nodebench"));
            s.write_all(
                b"Response: Success\r\nActionID: nb-1\r\nMessage: Authentication accepted\r\n\r\n",
            )
            .unwrap();
            let cmd = read_req(&mut r);
            assert!(cmd.contains("Command: rpt nodes 1999"));
            s.write_all(b"Event: FullyBooted\r\nStatus: Fully Booted\r\n\r\n")
                .unwrap();
            s.write_all(b"Response: Success\r\nActionID: nb-2\r\nMessage: Command output follows\r\nOutput: <NONE>\r\n\r\n")
                .unwrap();
            s.write_all(b"Event: RPT_LINKS\r\nNode: 1999\r\n\r\n")
                .unwrap();
            // Hold the socket open until the client is done.
            let mut sink = Vec::new();
            let _ = r.read_to_end(&mut sink);
        });
        let mut c = AmiClient::connect(addr, Duration::from_secs(5)).unwrap();
        assert_eq!(c.banner, "Asterisk Call Manager/9.0.0");
        c.login("nodebench", "not-a-real-secret", true).unwrap();
        assert_eq!(c.command("rpt nodes 1999").unwrap(), "<NONE>\n");
        let e1 = c.next_event(Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(e1.get("Event"), Some("FullyBooted"));
        let e2 = c.next_event(Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(e2.get("Event"), Some("RPT_LINKS"));
        assert!(c.next_event(Duration::from_millis(50)).unwrap().is_none());
        drop(c);
        srv.join().unwrap();
    }

    #[test]
    fn login_refusal_does_not_echo_secret() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(b"Asterisk Call Manager/9.0.0\r\n").unwrap();
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf).unwrap();
            s.write_all(
                b"Response: Error\r\nActionID: nb-1\r\nMessage: Authentication failed\r\n\r\n",
            )
            .unwrap();
        });
        let mut c = AmiClient::connect(addr, Duration::from_secs(5)).unwrap();
        let e = c.login("nodebench", "swordfish", false).unwrap_err();
        let text = e.to_string();
        assert!(text.contains("Authentication failed"));
        assert!(!text.contains("swordfish"));
        srv.join().unwrap();
    }
}
