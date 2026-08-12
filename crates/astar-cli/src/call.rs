// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Shared call driver for the `dial` and `parrot` subcommands. Both place a
//! single outbound call through the [`Manager`] with real audio devices, route
//! the mic, then run an event/PTT loop on the main thread: stdin PTT commands
//! key/unkey the call and call-progress events are printed as they arrive.

use std::time::Duration;

use astar_audio::{CpalBackend, Direction, MicId, OutputId};
use astar_iax::manager::DialSpec;
use astar_iax::{CallEvent, CallId, CallMode, CodecPolicy, Manager};

use crate::audio::resolve_device;
use crate::ptt::{self, PttCommand};

/// Common configuration parsed by the `dial` / `parrot` subcommands.
pub struct CallOptions {
    pub peer: std::net::SocketAddr,
    pub dest: String,
    pub caller_id: String,
    pub secret: String,
    pub input: Option<String>,
    pub output: Option<String>,
    /// Print the interactive PTT banner before entering the loop.
    pub ptt_prompt: bool,
}

/// Place the call and run the interactive event/PTT loop until the call hangs
/// up or stdin requests a quit.
pub fn run(opts: &CallOptions) -> Result<(), String> {
    let mut mgr = Manager::new(Box::new(CpalBackend::new()));
    let in_id = resolve_device(&mgr, opts.input.as_deref(), Direction::Input)?;
    let out_id = resolve_device(&mgr, opts.output.as_deref(), Direction::Output)?;

    let id = CallId::from_raw(1);
    mgr.dial(DialSpec {
        id,
        node: opts.dest.clone(),
        peer: opts.peer,
        output: OutputId::new(&out_id),
        caller_id: opts.caller_id.clone(),
        secret: opts.secret.clone(),
        mode: CallMode::Standard,
        dest: opts.dest.clone(),
        frame_observer: None,
        codec_policy: CodecPolicy::default(),
    })
    .map_err(|e| format!("dial failed: {e}"))?;
    mgr.route(id, &MicId::new(&in_id))
        .map_err(|e| format!("mic routing failed: {e}"))?;

    println!("dialing {} @ {}…", opts.dest, opts.peer);
    let events = mgr
        .take_events(id)
        .ok_or("call has no event stream (already taken?)")?;

    if opts.ptt_prompt {
        print_ptt_banner();
    }
    let ptt_rx = ptt::spawn_reader();

    let mut keyed = false;
    let mut answered = false;
    loop {
        // Drain lifecycle/media events.
        loop {
            match events.try_recv() {
                Ok(ev) => {
                    if handle_event(&ev) {
                        answered = true;
                    }
                    if let CallEvent::Hangup { .. } = ev {
                        // The far end (or local) tore the call down.
                        return Ok(());
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    println!("call ended (event stream closed)");
                    return Ok(());
                }
            }
        }

        // Drain PTT / control commands.
        loop {
            match ptt_rx.try_recv() {
                Ok(cmd) => {
                    if apply_ptt(&mut mgr, id, &mut keyed, answered, cmd) {
                        println!("hanging up…");
                        let _ = mgr.unkey(id);
                        mgr.hangup(id, None)
                            .map_err(|e| format!("hangup failed: {e}"))?;
                        return Ok(());
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // stdin thread gone without an explicit EOF command.
                    let _ = mgr.unkey(id);
                    mgr.hangup(id, None)
                        .map_err(|e| format!("hangup failed: {e}"))?;
                    return Ok(());
                }
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Apply one PTT command. Returns `true` when the loop should hang up and
/// exit (quit / EOF).
fn apply_ptt(
    mgr: &mut Manager,
    id: CallId,
    keyed: &mut bool,
    answered: bool,
    cmd: PttCommand,
) -> bool {
    match cmd {
        PttCommand::Key => set_ptt(mgr, id, keyed, true, answered),
        PttCommand::Unkey => set_ptt(mgr, id, keyed, false, answered),
        PttCommand::Toggle => {
            let next = !*keyed;
            set_ptt(mgr, id, keyed, next, answered);
        }
        PttCommand::Dtmf(c) => match mgr.send_dtmf(id, c) {
            Ok(()) => println!("sent DTMF {c}"),
            Err(e) => println!("DTMF {c} failed: {e}"),
        },
        PttCommand::Unknown(line) => {
            println!("? unknown command {line:?} — try k / u / t / d <digit> / q");
        }
        PttCommand::Quit | PttCommand::Eof => return true,
    }
    false
}

/// Set PTT to `on`, idempotently, printing the transition. Keying before the
/// call is answered is allowed (the engine buffers); we just note it.
fn set_ptt(mgr: &mut Manager, id: CallId, keyed: &mut bool, on: bool, answered: bool) {
    if *keyed == on {
        return;
    }
    let res = if on { mgr.key(id) } else { mgr.unkey(id) };
    match res {
        Ok(()) => {
            *keyed = on;
            if on {
                if answered {
                    println!("PTT: KEYED — talking");
                } else {
                    println!("PTT: KEYED (call not yet answered)");
                }
            } else {
                println!("PTT: unkeyed");
            }
        }
        Err(e) => println!("PTT change failed: {e}"),
    }
}

/// Print a call event. Returns `true` if it was the `Answered` event.
fn handle_event(ev: &CallEvent) -> bool {
    match ev {
        CallEvent::Ringing => {
            println!("event: ringing");
            false
        }
        CallEvent::Answered { format } => {
            println!("event: ANSWERED (codec {format:?}) — key PTT to talk");
            true
        }
        CallEvent::Dtmf(c) => {
            println!("event: inbound DTMF {c}");
            false
        }
        CallEvent::RemotePtt(on) => {
            println!(
                "event: remote PTT {}",
                if *on { "keyed" } else { "unkeyed" }
            );
            false
        }
        CallEvent::Text(t) => {
            println!("event: text {t:?}");
            false
        }
        CallEvent::ConnectionLost => {
            println!("event: connection lost (silence) — recovering");
            false
        }
        CallEvent::ConnectionRestored => {
            println!("event: connection restored");
            false
        }
        CallEvent::Hangup { reason } => {
            println!("event: HANGUP ({reason:?})");
            false
        }
        other => {
            // CallEvent is #[non_exhaustive]; print any future variant.
            println!("event: {other:?}");
            false
        }
    }
}

fn print_ptt_banner() {
    println!(
        "\
keyboard PTT ready (type a command + Enter):
  k / key / 1      engage PTT
  u / unkey / 0    release PTT
  t / toggle / <empty>   toggle PTT
  d <digit>        send DTMF
  q / quit         hang up and exit  (Ctrl-D also works)"
    );
}
