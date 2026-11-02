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
| EchoLink: dial from the same row as every network (a callsign or node number, resolved against the logged-in station list; Connect stays off with a reason and a Refresh List button when it isn't listed), a login line under the dial row (Log in / Log out with what Settings saved, or a button to Settings → EchoLink), browse, search and refresh the station list in a station-browser pane behind the magnifying glass (choosing fills the dial field, never connects), star stations there, on the dial row or on a call (all the same shared favorite; a Favorites filter shows them with live free/busy status, and those not online greyed), call a station, talk with the usual PTT (EchoLink stops transmitting after 10 minutes of continuous talk; the talk timer warns before that), chat; Settings → EchoLink holds the callsign, the Keychain password (shown on request), Log in automatically, the location text, log in / log out and Clear | yes | paused | yes — `[echolink]`, dial from `/admin`; stops sending the net to a station after 10 minutes without a pause | [macOS → EchoLink](site/macos/echolink.md), [astar-server → EchoLink](site/server/echolink.md) |
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
