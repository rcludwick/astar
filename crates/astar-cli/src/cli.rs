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
    ysf-listen     Link to a YSFReflector and decode RX audio (receive only;
                   requires a ThumbDV dongle and `--features ysf`).
    nxdn-listen    Link to an NXDNReflector talkgroup and decode RX audio
                   (receive only; requires a ThumbDV dongle and
                   `--features nxdn`).
    dmr-listen     Log in to a DMR master, join a talkgroup on a timeslot,
                   and decode RX audio (receive only; requires a ThumbDV
                   dongle and `--features dmr`). The master password comes
                   from ASTAR_DMR_PASSWORD, never from the command line.
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
    --reflector <CS>    Destination reflector callsign (e.g. XLX836), which
                         fills the transmitted RF header's RPT1/RPT2. Derived
                         from <host> when omitted; pass it when connecting by
                         bare IP address, which has nothing to derive from.
    -h, --help          Print this help and exit.

D-Star is hardware-only: a ThumbDV USB dongle must be attached (no software
fallback). Links, prints \"linked <host> module <M> (backend: thumbdv)\",
then a \"▶ <callsign>\" line per received transmission (with slow-data text
appended once it arrives).

Manual TX: type key/k to transmit, unkey/u to release, t to toggle, q to
quit (press Enter after each). Starts UNKEYED; nothing transmits until you
type key. Ctrl-C unlinks cleanly and exits.
";

/// `ysf-listen` usage. Only compiled with `--features ysf`.
#[cfg(feature = "ysf")]
pub const YSF_LISTEN_USAGE: &str = "\
astar-cli ysf-listen — link to a YSFReflector and decode RX audio

USAGE:
    astar-cli ysf-listen [OPTIONS] <host>

ARGS:
    <host>      Reflector host, or host:port. YSF standardises no port, so
                 the reflector's own is the one that matters; 42000 is the
                 most common and is used when none is given.

OPTIONS:
    --port <u16>        Reflector UDP port. Equivalent to host:port; giving
                         both is an error if they disagree.
    --callsign <CS>     This station's callsign (required).
    --wav <path>        Write decoded audio as an 8 kHz s16 mono WAV file at
                         <path> instead of playing it on the default output
                         device.
    --options <TEXT>    YCS room request. Omit for a plain YSFReflector.
    -h, --help          Print this help and exit.

RECEIVE ONLY. This command has no PTT; keying is the operator's, in the app.

YSF is hardware-only: a ThumbDV USB dongle must be attached (AMBE+2, no
software fallback). Links, prints \"linked <host>:<port> (backend: thumbdv)\",
then a \"▶ <callsign>\" line per received transmission.

astar decodes DN — V/D modes 1 and 2, what Yaesu radios transmit. A reflector
sending VW (full-rate voice) or data frames prints a \"!!\" line saying so
rather than producing silence with no explanation. Ctrl-C unlinks cleanly.
";

/// `nxdn-listen` usage. Only compiled with `--features nxdn`.
#[cfg(feature = "nxdn")]
pub const NXDN_LISTEN_USAGE: &str = "\
astar-cli nxdn-listen — link to an NXDNReflector talkgroup and decode RX audio

USAGE:
    astar-cli nxdn-listen [OPTIONS] <host> --radio-id <id> --tg <tg>

ARGS:
    <host>      Reflector host, or host:port. 41400 is the port the large
                 majority of the directory's NXDN rows publish, and is used
                 when none is given; the reflector's own always wins.

OPTIONS:
    --port <u16>        Reflector UDP port. Equivalent to host:port; giving
                         both is an error if they disagree.
    --callsign <CS>     This station's callsign (required). Rides in the
                         poll, not in a voice frame — NXDN addresses
                         stations by number on the wire.
    --radio-id <u16>    This station's NXDN radio id, 1-65535 (required).
                         A registration, not a default: 0 is refused.
    --tg <u16>          Talkgroup to join, 1-65535 (required). An
                         NXDNReflector answers only polls carrying its own
                         talkgroup, so a wrong or missing one links to
                         nothing.
    --wav <path>        Write decoded audio as an 8 kHz s16 mono WAV file at
                         <path> instead of playing it on the default output
                         device.
    -h, --help          Print this help and exit.

RECEIVE ONLY. This command has no PTT; astar has no NXDN transmit path yet.

NXDN is hardware-only: a ThumbDV USB dongle must be attached (AMBE+2, no
software fallback). Links, prints \"linked <host>:<port> (backend: thumbdv)\",
then a \"▶ <id>\" line per received transmission — a numeric id, not a
callsign, because NXDN carries no callsign on the wire for anyone but the
polling client itself. Ctrl-C unlinks cleanly.
";

/// `dmr-listen` usage. Only compiled with `--features dmr`.
///
/// There is no `--password` here, and there never will be: a secret in `argv`
/// is readable by every process on the machine and lands in shell history.
/// `ASTAR_DMR_PASSWORD` or nothing.
#[cfg(feature = "dmr")]
pub const DMR_LISTEN_USAGE: &str = "\
astar-cli dmr-listen — log in to a DMR master, join a talkgroup, decode RX audio

USAGE:
    ASTAR_DMR_PASSWORD=<pass> \\
      astar-cli dmr-listen [OPTIONS] <host> --system <net> --callsign <CS> \\
      --radio-id <id> --tg <tg>

ARGS:
    <host>      Master host, or host:port. 62031 is the homebrew convention
                 and is used when none is given; the network's own published
                 port always wins.

OPTIONS:
    --port <u16>        Master UDP port. Equivalent to host:port; giving both
                         is an error if they disagree.
    --system <name>     Which DMR this is (required). A talkgroup number names
                         nothing on its own — TG 91 is a different room on
                         every one of these networks — so the network is part
                         of the address, not a label. Any non-empty name is
                         accepted and passed through verbatim: an engine
                         family slug (tgif, freedmr, dmrplus, systemx,
                         amcomm, vkdmr, freestar, adn) or a directory server
                         name (freedmr-network, ipsc2-poland, xlx696). Only
                         BrandMeister is refused, and only because this build
                         cannot reach it at all.
    --callsign <CS>     This station's callsign (required). Rides in the
                         login, not in a voice burst — DMR addresses stations
                         by number on the wire.
    --radio-id <u32>    This station's DMR radio ID from radioid.net, 24 bits
                         (required). A registration, not a default: 0 and
                         anything past 16777215 are refused.
    --tg <u32>          Talkgroup to join (required). A master routes by the
                         room you joined, so there is no default to guess.
    --ts <1|2>          Timeslot. Defaults to 2, the hotspot convention — a
                         convention, not a specification.
    --wav <path>        Write decoded audio as an 8 kHz s16 mono WAV file at
                         <path> instead of playing it on the default output
                         device.
    -h, --help          Print this help and exit.

ENVIRONMENT:
    ASTAR_DMR_PASSWORD  The master's password (required). There is no flag
                         that takes it and there will not be one: a secret in
                         argv is readable by every process on the machine and
                         ends up in shell history. It is used for one login
                         digest and dropped; nothing stores or prints it.

RECEIVE ONLY. This command has no PTT; astar has no DMR transmit path yet.

Try it against your own machine first — `just dmr-parrot` runs a DMR master on
127.0.0.1 that does the real login handshake, so a wrong digest fails there
rather than against somebody's network:

    just dmr-parrot 62031
    ASTAR_DMR_PASSWORD=passw0rd just dmr-listen 127.0.0.1:62031 tgif KC0ABC \\
      3153591 31313

DMR is hardware-only: a ThumbDV USB dongle must be attached (AMBE+2 at
2450 + 1150, no software fallback). Logs in, prints
\"linked <host>:<port> (backend: thumbdv)\", then a \"▶ <id>\" line per received
transmission — a numeric id, not a callsign, because a DMRD carries srcId and
no callsign at all. Ctrl-C logs out cleanly.

BrandMeister is refused by the engine on this build, opt-in or not: it is a
private network whose operators set the terms, and astar has not confirmed
where they stand on third-party clients. See docs/design/dmr-networks.md.
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
