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

## Recording

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| Record a connection (button, Always record, three layouts, codec frames) | yes | paused | — | [macOS → Recording](site/macos/recording.md) |
| Record every transmission on the node, admin page, tarball for unamblify | — | — | yes | [astar-server → Recording](site/server/recording.md) |

## Digital voice

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| Vocoder trust: an AMBE vocoder stick (DVstick 30, ThumbDV, DV3000) is opened only once its USB serial is added; adding probes it with DTR/RTS held low after a CAT-cable warning, and a DVstick 33 (AMBE-3003) is recognised but not added yet | yes — Settings → Vocoders: list with connected dot and last seen, Add Vocoder…, Remove; kept out of exported configs | paused | yes — `node.toml` `[vocoder] trusted_serials`, `astar-server vocoder list` / `vocoder add`, settings-page card | [macOS → Vocoders (DVstick / ThumbDV)](site/macos/vocoders.md), [astar-server → Configuration](site/server/configuration.md#vocoder-trusted-ambe-vocoder-sticks) |
| D-Star vocoder choice: the dongle (the default choice) or Bruce Perens' software AMBE 3600×2400 vocoder, built into every shipped build (macOS app, server release), which needs no stick and opens no port; never a fallback either way. D-Star only: System Fusion, NXDN and DMR are AMBE+2 (US 8,359,197 until 2028-05-20) and stay dongle-only, and say why | yes — Settings → Vocoders: D-Star picker, "Dongle only" rows for the other three; with no stick, "Use Software for D-Star" in the empty list and "Use the Software Vocoder for D-Star" on a refused D-Star connect; Software greyed out with the reason only in an `ASTAR_DSTAR_SOFT_AMBE=0` build | paused | yes — `node.toml` `[vocoder] dstar` = `"dongle"` (default) or `"software"`, built into `just server-release` (a plain `cargo build` needs `--features dstar-soft-ambe`, else a start-up error), settings-page picker; no D-Star leg on the node yet | [macOS → Vocoders](site/macos/vocoders.md#which-vocoder-each-mode-uses), [astar-server → Configuration](site/server/configuration.md#vocoder-trusted-ambe-vocoder-sticks) |

## Settings

| Feature | macOS | Windows/Linux | astar-server | Docs |
|---|---|---|---|---|
| Collapsible Settings sections: every section of the Settings pane folds behind a disclosure triangle with its name and symbol; Expand All / Collapse All beside the title; Option-click a triangle to open or fold them all; a section header's "+" stays visible while folded and opens the section; links into Settings (Open Vocoder Settings…, Open EchoLink Settings…) open their section before scrolling; which sections are open is remembered across launches (Operator and Account open at first) and never travels in an exported config; each header is a VoiceOver button that says expanded or collapsed, and Space toggles it | yes | paused | — | [macOS → Finding your way around Settings](site/macos/index.md#finding-your-way-around-settings) |
