// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Hand-rolled argument parser. The workspace has no `clap` dependency (checked
//! every crate's `Cargo.toml`), and the existing example binaries hand-roll a
//! small `while let Some(arg)` loop, so this matches house style instead of
//! pulling in a new dep.

use std::net::SocketAddr;

/// Top-level usage text. Also printed by `--help` / `-h`.
pub const USAGE: &str = "\
astar-cli — command-line IAX2 client (register / dial / parrot / dstar-listen)

USAGE:
    astar-cli <COMMAND> [OPTIONS]

COMMANDS:
    register       Register to an IAX2 peer and report registration status.
    dial           Place a call, drive the FSM to answered, stream audio.
    parrot         Call a parrot/echo extension and loop audio back.
    dstar-listen   Link to a D-Star DExtra reflector module, decode RX
                   audio, and key manual TX from stdin (built with
                   `--features dstar`).

Run `astar-cli <COMMAND> --help` for command-specific options.

GLOBAL OPTIONS:
    -h, --help      Print this help and exit.

KEYBOARD PTT (dial / parrot):
    PTT is driven from stdin, one line per command (press Enter):
        k | key | 1     engage PTT  (mic audio flows to the peer)
        u | unkey | 0   release PTT
        t | toggle      flip PTT state
        <empty line>    toggle PTT (quick push-to-talk)
        d <digit>       send a DTMF digit (e.g. `d 5`)
        q | quit | hangup   hang up and exit
    EOF on stdin (Ctrl-D) also hangs up and exits.
";

pub const REGISTER_USAGE: &str = "\
astar-cli register — register to an IAX2 peer and report status

USAGE:
    astar-cli register [OPTIONS] <host[:port]> <user> [password]

ARGS:
    <host[:port]>   Registrar address. Port defaults to 4569 if omitted.
    <user>          Registration username.
    [password]      Secret. May also be given with --password.

OPTIONS:
    --password <s>      Secret (alternative to the positional argument).
    --refresh <secs>    Requested registration refresh interval (default 60).
    --timeout <secs>    Give up waiting for a terminal result (default 15).
    -h, --help          Print this help and exit.
";

pub const DIAL_USAGE: &str = "\
astar-cli dial — place a call and stream audio with keyboard PTT

USAGE:
    astar-cli dial [OPTIONS] <host[:port]> <number>

ARGS:
    <host[:port]>   Peer address. Port defaults to 4569 if omitted.
    <number>        Destination number / extension to dial.

OPTIONS:
    --caller-id <s>     Caller id sent in NEW (default \"astar\").
    --secret <s>        Call secret / password (default empty).
    --input <substr>    Capture device name substring (default: system default).
    --output <substr>   Playback device name substring (default: system default).
    --list-devices      List audio devices and exit.
    --no-ptt-prompt     Suppress the interactive PTT banner.
    -h, --help          Print this help and exit.

PTT is read from stdin; see the top-level --help for the control protocol.
";

pub const PARROT_USAGE: &str = "\
astar-cli parrot — call a parrot/echo extension and loop audio back

USAGE:
    astar-cli parrot [OPTIONS] [host[:port]] [number]

ARGS:
    [host[:port]]   Peer address (default 127.0.0.1:4569).
    [number]        Parrot/echo extension (default \"55553\", the ASL3 parrot).

OPTIONS:
    --caller-id <s>     Caller id sent in NEW (default \"astar\").
    --secret <s>        Call secret / password (default empty).
    --input <substr>    Capture device name substring (default: system default).
    --output <substr>   Playback device name substring (default: system default).
    --list-devices      List audio devices and exit.
    --no-ptt-prompt     Suppress the interactive PTT banner.
    -h, --help          Print this help and exit.

Key PTT (see top-level --help), talk, then unkey; the parrot echoes your audio
back through the selected output device.
";

/// `dstar-listen` usage. Only compiled with `--features dstar` — nothing
/// references it otherwise, and `astar_station`/`AmbeBackend`/etc. (named
/// in its body's neighboring code, not this string) aren't available either.
#[cfg(feature = "dstar")]
pub const DSTAR_LISTEN_USAGE: &str = "\
astar-cli dstar-listen — link to a D-Star DExtra reflector and decode RX audio

USAGE:
    astar-cli dstar-listen [OPTIONS] <host> <module>

ARGS:
    <host>      Reflector hostname or IP address.
    <module>    Reflector module letter (e.g. B).

OPTIONS:
    --port <u16>        Reflector UDP port (default 30001).
    --callsign <CS>     This station's callsign (required).
    --wav <path>        Write decoded audio as an 8 kHz s16 mono WAV file at
                         <path> instead of playing it on the default output
                         device.
    -h, --help          Print this help and exit.

D-Star is hardware-only: a ThumbDV USB dongle must be attached (no software
fallback). Links, prints \"linked <host> module <M> (backend: thumbdv)\",
then a \"▶ <callsign>\" line per received transmission (with slow-data text
appended once it arrives).

Manual TX: type key/k to transmit, unkey/u to release, t to toggle, q to
quit (press Enter after each). Starts UNKEYED; nothing transmits until you
type key. Ctrl-C unlinks cleanly and exits.
";

/// Resolve `host` (optionally `host:port`) to a `SocketAddr`, defaulting the
/// port to the IAX2 well-known 4569 when none is supplied. Delegates to the
/// shared [`astar_asl3::resolve_addr`] so the parsing lives in one place.
pub fn resolve_peer(host: &str) -> Result<SocketAddr, String> {
    astar_asl3::resolve_addr(host).map_err(|e| e.to_string())
}

/// Pull the value for a flag that expects an argument, mapping a missing value
/// to a usage error.
pub fn flag_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("flag {flag} requires a value"))
}
