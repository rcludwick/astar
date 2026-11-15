# Features

This is the complete list of what astar does **today**: every user-visible
feature of the macOS app, the Windows/Linux client and `astar-server`, plus
the engine capabilities they stand on, one row each, with which platforms
have it, so a gap between clients is visible at a glance. Shipped features
only: designs and backlog items go in only once the code lands, and when this
list and the code disagree, the code wins. "paused" means the Windows/Linux
client is paused (2026-10-08) and the feature is owed to its `astar-uiclone`
replacement; "—" means the feature does not belong on that platform. A "no"
is a defect with a backlog item. The list is not published; each row links to
its page on the public site. See CLAUDE.md, "Feature list", for the rules.

## Networks

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| EchoLink: dial from the same row as every network (a callsign or node number, resolved against the logged-in station list; Connect stays off with a reason and a Refresh List button when it isn't listed), a login line under the dial row (Log in / Log out with what Settings saved, or a button to Settings → EchoLink), browse, search and refresh the station list in a station-browser pane behind the magnifying glass (choosing fills the dial field, never connects), star stations there, on the dial row or on a call (all the same shared favorite; a Favorites filter shows them with live free/busy status, and those not online greyed), call a station, talk with the usual PTT (EchoLink stops transmitting after 10 minutes of continuous talk; the talk timer warns before that), chat; received audio plays out of the same adaptive jitter buffer as an AllStarLink call (the jitter-buffer switch and its Min/Max apply), so a busy conference that never pauses cannot build up delay, and the call-quality line under the meters reports it; Settings → EchoLink holds the callsign, the Keychain password (shown on request), Log in automatically, the location text, log in / log out and Clear | yes | paused | yes — `[echolink]`, dial from `/admin`; stops sending the net to a station after 10 minutes without a pause | [macOS → EchoLink](site/macos/echolink.md), [astar-server → EchoLink](site/server/echolink.md) |
| M17 reflector monitor on the node: `/dial m17` in Discord or `POST /m17` links one reflector module, receive-only toward the reflector (only connect, keepalive reply and disconnect are ever sent); off until `node.toml` has `[m17]`, which the settings page edits. The monitor contributes to the conference, so what the reflector carries is relayed to every AllStar node linked to the node and every EchoLink station on it, and a linked radio transmits it. Every place it is started or configured warns about that, with a count of who is connected at that moment: the admin page beside the links, the settings page's M17 card, the `POST /m17` reply (`relays_to`, `warning`), `/status` (`m17_relay`), Discord's dial reply and channel line, a log line when the reflector accepts, and the `[m17]` comments in `node.toml` | — | — | yes | [astar-server → Configuration, `[m17]`](site/server/configuration.md#m17-receive-only-m17-reflector-monitor) |
| GSM 06.10 on AllStar links (GSM-only nodes) | yes | paused | yes | [`codec_policy`](site/server/configuration.md) |

## Dialling

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| Favorites and recents shared across every network: one saved-nodes menu beside the dial field lists favorites and the last ten connects from AllStar, M17, D-Star, Fusion, NXDN, DMR and EchoLink, each marked with its network; picking one switches the network picker and fills the whole target (DMR's network, talkgroup and timeslot included) without connecting; the star saves the field's target on the selected network; entries are keyed by network and target, so EchoLink 1234 and AllStar 1234 are separate; Settings → Favorites lists and edits all of them with a network tag; they travel in an exported config's Node directory section | yes | paused | — | [macOS → Favorites and recents](site/macos/favorites.md) |
| Reflector directory search: the dial row's magnifying glass on a reflector network opens a search pane over the cached reflector directory (name, number, alias, description, country, sponsor), with a network filter listing only networks the directory has; un-dialable rows show greyed with the reason; choosing a row fills the dial field and switches the network picker to the row's network, never connects, and on D-Star asks for the module first; while EchoLink is logged in the filter offers EchoLink and All networks includes the EchoLink station list (callsign, location or node-number prefix; each row shows node, location and free/busy), choosing a station fills its callsign and switches to EchoLink, the rows follow list refreshes, and logging out removes them and drops an EchoLink filter back to All networks | yes | paused | — | [macOS → Searching the reflector directory](site/macos/reflectors.md) |
| AllStar node directory: the directory search pane gains an AllStar filter, and All networks includes AllStar nodes once there is a query; search by node number, callsign, town or frequency, and AllStar node numbers show with their callsign and location (in recents, favorites and the call header); the credit shows wherever AllStar rows do; the data comes from hamcall-db, which refreshes nightly, and the app refreshes its copy weekly | yes | — | — | [macOS → Searching the reflector directory](site/macos/reflectors.md) |

## Recording

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| Record a connection (button, Always record, three layouts, codec frames) | yes | paused | — | [macOS → Recording](site/macos/recording.md) |
| Record every transmission on the node, admin page, tarball for unamblify | — | — | yes | [astar-server → Recording](site/server/recording.md) |
| Packet capture for debugging: a Debug mode switch adds a capture button that writes received media packets, on every network, to a pcap for Wireshark with a jitter-buffer history beside it; nothing is captured until it's pressed; the app captures the current call until pressed again or the call ends, and the server captures all live calls until pressed again; size-limited | yes | paused | yes | [Packet logs](site/reference/packet-logs.md) |
| Link log: every AllStarLink link the node makes or answers writes `info` lines on the target `astar_iax::link` as it starts (direction, far address, claimed node number; an inbound call only once it has echoed the call token, so a forged source writes nothing at `info`), settles its codec, comes up and ends (who hung up: us, them or a time-out; the cause text; how long it lasted), plus a `warn` when a link ends less than 10 s after coming up; never a secret or call token, caller-supplied text cut to 64 characters with control characters as `?`, nothing per audio packet; read with `podman logs` or journald | — (the engine writes the same lines for the app's calls, but the app shows no log) | paused | yes | [astar-server → Reading the link log](site/server/index.md#reading-the-link-log) |
| Record scheduled M17 and D-Star nets, listen-only, as session archives: one whole-session FLAC, one FLAC per over, a log (JSON and CSV), a check-in list, a waveform file (`peaks.json`) and `session.json` per night, with over files and the check-in list named by base call, joined as `<call> R` shortly before each net and left after it; retention by size and age; D-Star on a trusted dongle or the software vocoder. A separate daemon, `astar-netrec`, on Linux and macOS; it is not a client feature and not part of `astar-server` | — | — | — | [Net recorder](site/netrec.md) |
| Browse recorded nets as a conversation: a list, a session page with one play button, bubbles per over, short key-ups as chips, a "My side" switch, a full-screen replay view, `#t=` links, and delete for finished sessions. The page is the UI, in any browser; `astar-netrec` serves it on 127.0.0.1 (`[web] local`) and `astar-server` serves it at `/nets/` (`[netrec] archive`), public sessions to anyone and private ones after sign-in | — | — | yes | [Net recorder](site/netrec.md), [Net recordings](site/server/nets.md) |

## Digital voice

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| Vocoder trust: an AMBE vocoder stick (DVstick 30, ThumbDV, DV3000) is opened only once its USB serial is added; adding probes it with DTR/RTS held low after a CAT-cable warning, and a DVstick 33 (AMBE-3003) is recognised but not added yet | yes — Settings → Vocoders: list with connected dot and last seen, Add Vocoder…, Remove; kept out of exported configs | paused | yes — `node.toml` `[vocoder] trusted_serials`, `astar-server vocoder list` / `vocoder add`, settings-page card | [macOS → Vocoders (DVstick / ThumbDV)](site/macos/vocoders.md), [astar-server → Configuration](site/server/configuration.md#vocoder-trusted-ambe-vocoder-sticks) |
| D-Star vocoder choice: the dongle (the default choice) or Bruce Perens' software AMBE 3600×2400 vocoder, built into every shipped build (macOS app, server release), which needs no stick and opens no port; never a fallback either way. D-Star only: System Fusion, NXDN and DMR are AMBE+2 (US 8,359,197 until 2028-05-20) and stay dongle-only, and say why | yes — Settings → Vocoders: D-Star picker, "Dongle only" rows for the other three; with no stick, "Use Software for D-Star" in the empty list and "Use the Software Vocoder for D-Star" on a refused D-Star connect; Software greyed out with the reason only in an `ASTAR_DSTAR_SOFT_AMBE=0` build | paused | yes — `node.toml` `[vocoder] dstar` = `"dongle"` (default) or `"software"`, built into `just server-release` (a plain `cargo build` needs `--features dstar-soft-ambe`, else a start-up error), settings-page picker; no D-Star leg on the node yet | [macOS → Vocoders](site/macos/vocoders.md#which-vocoder-each-mode-uses), [astar-server → Configuration](site/server/configuration.md#vocoder-trusted-ambe-vocoder-sticks) |

## Settings

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| Collapsible Settings sections: every section of the Settings pane folds behind a disclosure triangle with its name and symbol; Expand All / Collapse All beside the title; Option-click a triangle to open or fold them all; a section header's "+" stays visible while folded and opens the section; links into Settings (Open Vocoder Settings…, Open EchoLink Settings…) open their section before scrolling; which sections are open is remembered across launches (Operator and Account open at first) and never travels in an exported config; each header is a VoiceOver button that says expanded or collapsed, and Space toggles it | yes | paused | — | [macOS → Finding your way around Settings](site/macos/index.md#finding-your-way-around-settings) |

## Audio

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| Audio device recovery: a speaker or mic that changes format mid-call (a Bluetooth headset going hands-free) is reopened on the new format, and one that cannot be reopened is named ("Speaker stopped: …") | yes | paused (the engine still reopens the stream; no message is shown) | yes in the default build, where the engine reopens the stream (no UI); the static release has no sound devices | [Hardware](site/macos/hardware.md#speaker-stopped-and-microphone-stopped) |
| Parrot mode on the node (`[bridge] mode = "parrot"`): each caller's over is recorded, pauses between words kept, and played back to that caller alone after `playback_delay_ms`, then a spoken signal report; a caller from the app or a plain IAX2 client is then hung up, and an AllStarLink node that linked in stays linked so it can talk again | — | — | yes | [astar-server → Configuration, `[parrot]`](site/server/configuration.md#parrot-parrot-tuning) |

## Push-to-talk

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| Bluetooth PTT button (Inrico B02PTT) | yes | paused | — (the node has no Bluetooth adapter; a speaker-mic's audio is on the operator's computer) | [macOS → Bluetooth PTT button](site/macos/bluetooth-ptt.md) |
| Transmit time-out: one app-wide limit on every keying source, 1–10 min, default 2, never off | yes | paused (the engine's 2 min default still applies to its keying; no setting, no banner) | yes, fixed at the 2 min default | [On-air safety](site/about/safety.md#the-transmit-time-out), [First run](site/macos/first-run.md#4-choose-how-you-key-up) |
| VOX: keys when the mic's 50 ms RMS loudness holds over the threshold for 120 ms, so clicks and pops shorter than ~70 ms never key; a 250 ms pre-roll sends the wait so the first word is whole; Test shows the same loudness against the threshold; a threshold saved against the old peak level is converted once (−10 dB) | yes | paused | — (the parrot's own VOX is separate and unchanged) | [macOS → How VOX hears you](site/macos/first-run.md#how-vox-hears-you) |
| Combined PTT sources: the on-screen button and Space, VOX, serial and the Bluetooth button combine; the on-screen button or Space stops (Space only, with VOX on) | yes | paused | — (one keying source, `POST /key`; `POST /unkey` stops it) | [On-air safety](site/about/safety.md#how-keying-is-limited) |
