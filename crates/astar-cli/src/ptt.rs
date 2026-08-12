// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Keyboard (stdin) PTT. A background thread reads stdin one line at a time and
//! turns each line into a [`PttCommand`] on an mpsc channel, so the call loop
//! can poll for control input alongside call events without blocking on the
//! terminal. Line-based (press Enter) — deliberately simple and portable; no
//! raw-mode/termios dependency.

use std::io::BufRead;
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// A control action parsed from one line of stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PttCommand {
    /// Engage PTT (mic flows to the peer).
    Key,
    /// Release PTT.
    Unkey,
    /// Flip the current PTT state.
    Toggle,
    /// Send a DTMF digit.
    Dtmf(char),
    /// Hang up and exit.
    Quit,
    /// A line we did not understand (echoed back as a hint).
    Unknown(String),
    /// stdin reached EOF (Ctrl-D). The reader thread is done.
    Eof,
}

/// Parse one trimmed line into a [`PttCommand`]. Empty line == toggle (a quick
/// push-to-talk tap).
fn parse_line(line: &str) -> PttCommand {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return PttCommand::Toggle;
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "k" | "key" | "1" => PttCommand::Key,
        "u" | "unkey" | "0" => PttCommand::Unkey,
        "t" | "toggle" => PttCommand::Toggle,
        "q" | "quit" | "hangup" | "exit" => PttCommand::Quit,
        _ => {
            // `d <digit>` (or `dtmf <digit>`) sends a DTMF tone.
            let mut parts = trimmed.split_whitespace();
            let head = parts.next().map(str::to_ascii_lowercase);
            if matches!(head.as_deref(), Some("d" | "dtmf"))
                && let Some(rest) = parts.next()
            {
                let mut chars = rest.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    return PttCommand::Dtmf(c);
                }
            }
            PttCommand::Unknown(trimmed.to_string())
        }
    }
}

/// Spawn the stdin reader thread. Returns a receiver of [`PttCommand`]s. The
/// thread exits (sending [`PttCommand::Eof`]) when stdin closes.
#[must_use]
pub fn spawn_reader() -> Receiver<PttCommand> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("iax-cli-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let cmd = match line {
                    Ok(l) => parse_line(&l),
                    Err(_) => PttCommand::Eof,
                };
                let is_terminal = matches!(cmd, PttCommand::Eof);
                if tx.send(cmd).is_err() || is_terminal {
                    return;
                }
            }
            let _ = tx.send(PttCommand::Eof);
        })
        .expect("spawn stdin reader");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_aliases() {
        for s in ["k", "key", "1", "  KEY  ", "Key"] {
            assert_eq!(parse_line(s), PttCommand::Key, "input {s:?}");
        }
    }

    #[test]
    fn parses_unkey_aliases() {
        for s in ["u", "unkey", "0", "UNKEY"] {
            assert_eq!(parse_line(s), PttCommand::Unkey, "input {s:?}");
        }
    }

    #[test]
    fn empty_line_toggles() {
        assert_eq!(parse_line(""), PttCommand::Toggle);
        assert_eq!(parse_line("   "), PttCommand::Toggle);
        assert_eq!(parse_line("toggle"), PttCommand::Toggle);
    }

    #[test]
    fn parses_quit() {
        for s in ["q", "quit", "hangup", "exit"] {
            assert_eq!(parse_line(s), PttCommand::Quit, "input {s:?}");
        }
    }

    #[test]
    fn parses_dtmf() {
        assert_eq!(parse_line("d 5"), PttCommand::Dtmf('5'));
        assert_eq!(parse_line("dtmf #"), PttCommand::Dtmf('#'));
        // multi-char operand is not a DTMF digit
        assert_eq!(parse_line("d 55"), PttCommand::Unknown("d 55".to_string()));
    }

    #[test]
    fn unknown_passthrough() {
        assert_eq!(
            parse_line("frobnicate"),
            PttCommand::Unknown("frobnicate".to_string())
        );
    }
}
