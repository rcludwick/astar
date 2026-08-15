// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! stdin/TUI menu adapter for astar-server.
//!
//! Provides a line-based interactive menu that maps single-character commands
//! to [`NodeCommand`]s. No raw-mode / termios dependency — press Enter after
//! each key. The pure parsing logic lives in [`parse_menu_line`] and is fully
//! unit-testable without any stdin access.

use std::io::BufRead;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crate::{
    command::{NodeCommand, NodeEvent},
    controller::NodeController,
};

/// Parse one line of stdin into a [`NodeCommand`].
///
/// Trims leading/trailing whitespace. Returns `None` for empty lines and
/// unrecognised input. Case-sensitive where the brief distinguishes (`g` vs
/// `G`).
///
/// # Mapping
/// | Input       | Command                       |
/// |-------------|-------------------------------|
/// | `d <node>`  | `Dial { node }`               |
/// | `h`         | `Hangup`                      |
/// | `k`         | `Key`                         |
/// | `u`         | `Unkey`                       |
/// | `a`         | `Answer`                      |
/// | `r`         | `Reject`                      |
/// | `i`         | `EnableInbound`               |
/// | `o`         | `DisableInbound`              |
/// | `g`         | `Register`                    |
/// | `G`         | `Deregister`                  |
/// | `s`         | `Status`                      |
/// | `q`         | `Shutdown`                    |
/// | empty / ?   | `None`                        |
pub fn parse_menu_line(line: &str) -> Option<NodeCommand> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let rest = parts.next().map(str::trim);

    match head {
        "d" => {
            let node = rest?.to_string();
            if node.is_empty() {
                None
            } else {
                Some(NodeCommand::Dial { node })
            }
        }
        "h" => Some(NodeCommand::Hangup),
        "k" => Some(NodeCommand::Key),
        "u" => Some(NodeCommand::Unkey),
        "a" => Some(NodeCommand::Answer),
        "r" => Some(NodeCommand::Reject),
        "i" => Some(NodeCommand::EnableInbound),
        "o" => Some(NodeCommand::DisableInbound),
        "g" => Some(NodeCommand::Register),
        "G" => Some(NodeCommand::Deregister),
        "s" => Some(NodeCommand::Status),
        "q" => Some(NodeCommand::Shutdown),
        _ => None,
    }
}

/// Spawn a background stdin-reader thread. Returns a [`Receiver<NodeCommand>`].
///
/// Each line read from stdin is passed through [`parse_menu_line`]. Only
/// successfully-parsed commands are forwarded over the channel; unknown / empty
/// lines are silently skipped. The thread stops on EOF or after forwarding a
/// `Shutdown` command.
///
/// Mirrors the pattern in `crates/astar-cli/src/ptt.rs::spawn_reader`.
#[must_use]
pub fn spawn_reader() -> Receiver<NodeCommand> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("iax-node-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(text) = line else { return };
                if let Some(cmd) = parse_menu_line(&text) {
                    let is_shutdown = matches!(cmd, NodeCommand::Shutdown);
                    if tx.send(cmd).is_err() || is_shutdown {
                        return;
                    }
                }
            }
        })
        .expect("spawn iax-node-stdin reader");
    rx
}

/// Print the menu legend to stdout.
fn print_legend() {
    println!("astar-server TUI — commands (press Enter after each):");
    println!("  d <node>  Dial node");
    println!("  h         Hangup");
    println!("  k / u     Key / Unkey (PTT)");
    println!("  a / r     Answer / Reject inbound call");
    println!("  i / o     Enable / Disable inbound listener");
    println!("  g / G     Register / Deregister");
    println!("  s         Status (snapshot)");
    println!("  q         Shutdown and exit");
    println!();
}

/// Run an interactive TUI loop.
///
/// Prints the menu legend, then reads lines from stdin in a simple inline
/// loop. Each parsed command is dispatched to `ctrl.execute()`; the reply
/// (or error) is printed. Pending async events from `ctrl.subscribe()` are
/// drained after each command. Exits on `Shutdown` or EOF.
///
/// Not unit-tested (no stdin in tests); intentionally panic-free.
pub fn run_tui(ctrl: &NodeController) {
    print_legend();

    // Subscribe before the loop so we don't miss events from early commands.
    let events = ctrl.subscribe();

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(text) = line else { break };

        let Some(cmd) = parse_menu_line(&text) else {
            continue;
        };

        let is_shutdown = matches!(cmd, NodeCommand::Shutdown);

        match ctrl.execute(cmd) {
            Ok(reply) => match serde_json::to_string(&reply) {
                Ok(json) => println!("ok: {json}"),
                Err(_) => println!("ok"),
            },
            Err(err) => println!("error: {}", err.message),
        }

        // Drain pending events (non-blocking).
        ctrl.pump();
        while let Ok(ev) = events.try_recv() {
            print_event(&ev);
        }

        if is_shutdown {
            break;
        }
    }
}

/// Pretty-print a [`NodeEvent`] to stdout.
fn print_event(ev: &NodeEvent) {
    match ev {
        NodeEvent::Snapshot(snap) => {
            println!(
                "event: snapshot — listening={} registered={} calls={}",
                snap.listening,
                snap.registered,
                snap.calls.len()
            );
        }
        NodeEvent::IncomingCall { from } => {
            println!("event: incoming call from {from}");
        }
        NodeEvent::Registered => {
            println!("event: registered");
        }
        NodeEvent::RegisterFailed { reason } => {
            println!("event: register failed — {reason}");
        }
        NodeEvent::Hangup { reason } => {
            println!("event: hangup — {reason}");
        }
        NodeEvent::AnnouncementStarted { kind } => {
            println!("event: announcement started ({kind})");
        }
        NodeEvent::AnnouncementFinished { kind } => {
            println!("event: announcement finished ({kind})");
        }
        NodeEvent::Link {
            kind, node, call, ..
        } => {
            println!("event: link {kind} — node {node} (call {call})");
        }
        NodeEvent::Dtmf {
            call,
            digit,
            command,
        } => match command {
            Some(cmd) => println!("event: dtmf {digit} (call {call}) → {cmd}"),
            None => println!("event: dtmf {digit} (call {call})"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Required test from the brief ---

    #[test]
    fn menu_line_parses() {
        assert!(
            matches!(parse_menu_line("d 55553"), Some(NodeCommand::Dial { node }) if node == "55553")
        );
        assert!(matches!(parse_menu_line("k"), Some(NodeCommand::Key)));
        assert!(matches!(parse_menu_line("q"), Some(NodeCommand::Shutdown)));
        assert!(parse_menu_line("").is_none());
        assert!(parse_menu_line("zzz").is_none());
    }

    // --- Additional thorough parse tests ---

    #[test]
    fn parses_hangup() {
        assert!(matches!(parse_menu_line("h"), Some(NodeCommand::Hangup)));
    }

    #[test]
    fn parses_unkey() {
        assert!(matches!(parse_menu_line("u"), Some(NodeCommand::Unkey)));
    }

    #[test]
    fn parses_answer_and_reject() {
        assert!(matches!(parse_menu_line("a"), Some(NodeCommand::Answer)));
        assert!(matches!(parse_menu_line("r"), Some(NodeCommand::Reject)));
    }

    #[test]
    fn parses_inbound_toggle() {
        assert!(matches!(
            parse_menu_line("i"),
            Some(NodeCommand::EnableInbound)
        ));
        assert!(matches!(
            parse_menu_line("o"),
            Some(NodeCommand::DisableInbound)
        ));
    }

    #[test]
    fn parses_register_case_sensitive() {
        // lowercase g → Register
        assert!(matches!(parse_menu_line("g"), Some(NodeCommand::Register)));
        // uppercase G → Deregister
        assert!(matches!(
            parse_menu_line("G"),
            Some(NodeCommand::Deregister)
        ));
    }

    #[test]
    fn parses_status() {
        assert!(matches!(parse_menu_line("s"), Some(NodeCommand::Status)));
    }

    #[test]
    fn dial_requires_node_arg() {
        // `d` with no argument → None
        assert!(parse_menu_line("d").is_none());
        // `d` with whitespace only → None
        assert!(parse_menu_line("d   ").is_none());
    }

    #[test]
    fn dial_trims_whitespace() {
        // Surrounding whitespace on the whole line is stripped.
        assert!(
            matches!(parse_menu_line("  d 12345  "), Some(NodeCommand::Dial { node }) if node == "12345")
        );
    }

    #[test]
    fn unknown_and_empty_return_none() {
        for line in ["", "  ", "?", "x", "HANGUP", "quit", "K", "Q"] {
            assert!(
                parse_menu_line(line).is_none(),
                "expected None for {line:?}"
            );
        }
    }

    #[test]
    fn shutdown_is_lowercase_q_only() {
        assert!(matches!(parse_menu_line("q"), Some(NodeCommand::Shutdown)));
        // Uppercase Q is not a recognised command.
        assert!(parse_menu_line("Q").is_none());
    }
}
