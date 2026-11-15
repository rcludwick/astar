# nodebench (developer page)

This page is for developers. It is not on the public site: it lives outside
`docs/site/` and is not in `zensical.toml`'s nav, because the bench is not
a user feature.

nodebench is astar-server's regression bench. It runs against a **stock
AllStarLink 3 node** in podman, so a passing check means astar-server
interoperates with real `app_rpt` rather than with our idea of it. It
exists because of `iax-cnlink`: Rob's ClearNode drops its link to
astar-server after one or two seconds, and no unit test or self-made peer
ever showed that.

| Where | What |
|---|---|
| [`harness/nodebench/README.md`](../harness/nodebench/README.md) | How to use it: quick start, flags, topologies, scenarios, reports, adding a scenario, CI, safety |
| [`harness/nodebench/RIG.md`](../harness/nodebench/RIG.md) | The rig's reference: images, ports, credentials, variants, and every finding with its evidence |
| `crates/astar-nodebench/` | The `nodebench` binary, with `publish = false` |
| `harness/nodebench/{pod,nat}.yaml`, `rig.sh`, `images/` | The podman side |
| `.github/workflows/ci.yml`, job `nodebench` | The CI job. It is off unless dispatched, because the runner cannot nest podman yet. |

## The shape of it

```
            host (the Mac, or the CI runner container)
  ┌───────────────────────────────────────────────────────────────┐
  │ nodebench (host binary)                                       │
  │   AMI / podman exec ──────────────┐   HTTP + SSE ──────┐      │
  │   USRP probe 127.0.0.1:34001 ◄─┐  │   IAX probe ─────┐ │      │
  │                                │  │                  │ │      │
  │   127.0.0.1:{32001,24569,25038}│  │ 127.0.0.1:{24570,28730}   │
  └────────────────────────────────┼──┼──────────────────┼─┼──────┘
        pod "nodebench" (one netns; services talk on 127.0.0.1)
  ┌────────────────────────────────┼──┼──────────────────┼─┼──────┐
  │ usrp-relay ◄──► asl3 (1999) ◄──IAX2 127.0.0.1──► astar-server │
  │                 app_rpt, USRP rxchannel           (1998)      │
  │ capture: tshark on lo + eth0 → pcap ring                      │
  └───────────────────────────────────────────────────────────────┘
```

- **The node.** `debian:12` plus the official ASL3 packages, configured
  only through config files. Its "radio" is a USRP channel that the bench
  drives.
- **astar-server.** Built from this checkout for the pod's own
  architecture. The Mac's podman VM is arm64.
- **The bench.**
  - It drives the node through AMI and `podman exec asterisk -rx`.
  - It drives astar-server through its control HTTP API and SSE.
  - It puts audio on both sides: USRP into the node, and a third IAX2 peer
    (`astar-iax`) on astar-server's bridge.
  - It reads the wire from the tshark sidecar, as decoded IAX2 full frames,
    and, for the speech echo, one direction's voice frames decoded to audio.

Scenarios are plain Rust, one per file in `src/scenario/`. Each returns
named checks, and each check passes, fails or skips with a reason. A failure
keeps the last 50 frames before the drop and the tails of both logs. See
README "Adding a scenario".

## Tiers

| Tier | Command | Status |
|---|---|---|
| Quick | `just nodebench` (`nodebench run`) | Built: 17 scenarios. Findings 1–8 are fixed (2026-10-12, `work/iax-fixes`), and so are the speech findings S1–S4 and S6 (2026-11-14, `work/link-proof`). |
| Soak | `nodebench soak --hours N [--topology loopback\|nat] [--udp-timeout S]` | Built alongside this page (W5). The `nat` topology runs on podman `--internal` networks. |
| Results page | `nodebench serve`, on http://127.0.0.1:8740 | Built alongside this page (W5) |
| Release | `just nodebench-release`: urfd/xlxd, mrefd, YSF/NXDN reflectors, DMR masters, each built from pinned upstream source | Designed, not built |
| App | the astar Bench build of the macOS app, driven over a loopback automation API | Designed, not built |

The design is
`~/dev/notes/docs/superpowers/designs/astar/2026-10-11-nodebench-design.md`.
It is in Rob's private notes, not in this repository.

## What it has found (2026-10-11), and the fixes (2026-10-12)

Every finding is astar-server behaviour, with frame-level evidence in
RIG.md. The node behaves as stock app_rpt does.

| # | Finding | Shows in | Status |
|---|---|---|---|
| 1 | astar-server's link NEW has no CALLING NUMBER, so app_rpt hangs up 0.6 ms after ACCEPT: "Caller ID is not numeric" | `server_links_to_node` | Fixed (`3c34307`, `CallMode::NodeLink`) |
| 2, 8 | Registration with call tokens storms (REGREQ, VNAK, repeat) and exhausts the node's 2048 call numbers | `register_with_node` | Fixed (`18eed6b`): the token REGREQ restarts at `oseqno` 0; one call number |
| 3 | After `/link disconnect` the node keeps the link for 10 s: app_rpt holds a link that hangs up without `!!DISCONNECT!!` for its `DISC_TIME` (the HANGUP's timestamp 0 was a second, smaller defect) | `disconnect_from_each_side`, `dtmf_link_command` | Fixed (`a77f81b`): clears in 0.2 s |
| 4 | ACKs carry `iseqno` = the acked frame's `oseqno`, not `oseqno + 1`. A call's last frame is never acknowledged, retransmitted frames are processed again, and a stale HANGUP on a reused call number kills the next link. **The `iax-cnlink` cause the bench reproduced.** | `relink_after_drop`, `dtmf_link_command` | Fixed (`626df4d`) |
| 5 | Random binary call tokens can contain NUL, which Asterisk truncates. About 6 % of inbound links get REJECT "Call Token Invalid". | `codec_matrix`, `auth_variants` | Fixed (`fb17eb8`, printable tokens) |
| 6 | astar-server never answers `!NEWKEY1!` (it compared app_rpt's C string with its NUL), so from 2 s into a link the node ignores its audio; once answered, the bridge's continuous silence kept the node keyed | `audio_server_to_node`, `node_links_to_server`, `server_links_to_node` | Fixed (`564d091`, `306bd92`) |
| 7 | Audio from a link that never keyed (a monitor link) is mixed into the bridge | `monitor_link` | Fixed (`dbeeeb6`): app_rpt's keying rule per leg |
| — | A NEW retransmitted without its token gets REJECT, not CALLTOKEN | `calltoken_retransmit` | Fixed (`ca622a2`) |

`docs/BACKLOG.md` `iax-cnlink` has the reasoning about which of these could
be Rob's drop.

## The speech echo (2026-11-13)

`parrot_echoes_speech` sends a spoken phrase through a node link into
astar-server's parrot, twice on one link, and compares each echo with it, on
the link and at the node's radio; the echo on the link must match the over
astar-server received. `speech_server_to_node` sends the phrase the other
way, from an IAX2 probe on astar-server's bridge to the node's radio.
RIG.md "Speech echo" has the thresholds, the measured runs and the
evidence.

| # | Finding | Status |
|---|---|---|
| S1 | The parrot's VOX take left out the quiet frames between words, so a 5.18 s phrase came back as 3.72 s with its pauses squeezed out | Fixed (`2ed1dd9`) |
| S2 | The parrot hung up the link after every echo (`!!DISCONNECT!!` 7 ms after the last echo frame), so a node that linked to it was dropped | Fixed (`803adb3`, Rob's ruling): an AllStarLink node link stays up after its echo; other callers are still hung up after their report |
| S3 | A conference member's input had no jitter cushion: a frame a few ms late missed its 20 ms tick, and the parrot recorded the silent tick, so the echo got a 20 ms stall or a 40 ms repeat the over did not have | Fixed for the parrot (`90efb10`: a take records only frames the caller sent); for the bridge, S6 |
| S4 | When astar-server ended a link with `!!DISCONNECT!!` and the node hung up inside the grace, the link log said `ended_by=them` | Fixed (`0abdc7c`): it says `us` |
| S5 | app_rpt's own parrot plays only to its own transmitter, never to its links, so there can be no "node parrot" echo scenario; `parrot = 2` does not even turn it on (it takes `parrot = yes`) | Not an astar-server issue; the scenario was dropped |
| S6 | The bridge's output to a node had a 20 ms stall or a 40 ms repeat or gap the probe's input did not, in about half the overs (`speech_server_to_node`). Same cause as S3: the conference read each member once per tick with no cushion | Fixed (`f68a869`, `37de502`, `338d37a`): a 40 ms per-member jitter cushion played by the sender's timestamps, a mixing thread that catches up a late tick, and link keying that waits for the cushion to drain. 20 of 20 overs clean since |

## Safety, in one paragraph

There is no radio, sound card or serial device anywhere in the rig. In the
quick tier, everything is on 127.0.0.1 inside one pod, and published ports
are on the host's 127.0.0.1 only. The soak tier's `nat` topology uses podman
`--internal` networks with no route out, which Rob approved on 2026-10-11 and
CLAUDE.md records. AllStar DNS is never queried. Nothing registers with the
portal, posts stats or downloads the node list. Node numbers 1999 and 1998
are private test numbers. `key_refusal` and the transmit time-out are
untouched. README "Safety model" has the whole list.

## CI

The `nodebench` job is ready, but **the IONOS runner cannot run it yet**:

- the runner image has no podman;
- the runner container's default confinement stops a nested rootless podman
  from mapping its user namespace.

README "CI" lists the exact image lines and container options. It also
covers the security cost of each option and the open questions.
`harness/nodebench/ci/nested-trial.sh` proves the recipe on the Mac: it runs
the real rig inside an Ubuntu 24.04 stand-in of the runner image. Changing
the box is a `gh-runners` change, and Rob's call.

## App automation contract

The design's app tier puts the automation contract here (verbs and JSON
shapes), so the macOS app and the later `astar-uiclone` clients implement
the same one. It is not defined yet; it will be written when that tier is
built.
