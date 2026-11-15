# nodebench rig

A stock AllStarLink 3 node (1999) and astar-server (1998) in one podman pod,
with a tshark sidecar. This page covers what the rig is, how to drive it, and
what the hand gate found on 2026-10-11. The design is
`~/dev/notes/docs/superpowers/designs/astar/2026-10-11-nodebench-design.md`.

No radio exists anywhere in the rig. The node's "radio" is a USRP channel,
and the bench drives it.

## Quick start

```bash
export CONTAINER_CONNECTION=podman-machine-default   # the Mac; arm64 VM
harness/nodebench/rig.sh build       # both images, native arch
harness/nodebench/rig.sh up          # ~1 s until both nodes answer
harness/nodebench/rig.sh status
harness/nodebench/hold.sh node 90    # ASL3 dials 1998, hold 90 s
harness/nodebench/hold.sh server 90  # astar-server dials 1999, hold 90 s
harness/nodebench/rig.sh down
```

## Files

| Path | What |
|---|---|
| `images/asl3/Containerfile` | `debian:12` plus the official ASL3 apt repo (`asl-apt-repos.deb12_all.deb`), installing `asl3-asterisk` only |
| `images/asl3/conf/` | The bench's Asterisk config. It replaces the stock files outright. |
| `images/asl3/entrypoint.sh` | Renders `iax.conf` from `iax.conf.in` with the codec list, optionally turns on debug, then `exec asterisk -f` |
| `images/astar-server/Containerfile` | A multi-stage `rust:1-bookworm` build of `-p astar-server` from this checkout, run on `debian:12-slim`. The context is the repo root. |
| `images/astar-server/Containerfile.ignore` | Trims the build context. Passed with `--ignorefile`. |
| `images/astar-server/node.toml` | The default astar-server config |
| `images/astar-server/node-register.toml` | The same config, but it registers with the ASL3 node (`--register`) |
| `pod.yaml` | The loopback topology for `podman kube play`. `rig.sh` fills in the `@...@` placeholders. |
| `rig.sh` | `build`, `up`, `down`, `status`, `logs`, `cli`, `full-log` |
| `hold.sh` | Hand gate: dials from one side, polls both sides once a second, and reports when the link drops |
| `findings/` | Gate artifacts: logs, IAX full-frame pcaps, and summaries. No secrets. |
| `captures/` | Live tshark ring buffer (gitignored) |
| `crates/astar-nodebench/fixtures/link-test-phrase.wav` | The spoken test phrase (eSpeak NG, 8 kHz mono, 5.2 s) the speech-echo scenario (`parrot_echoes_speech`) sends; its README records how it was made and its licence. `astar_nodebench::speech::compare` lines an echo up against it. |
| `<repo>/target/nodebench/` or `<repo>/.nodebench/` | Run output from `nodebench run`/`soak`, and what `nodebench serve` reads. `.nodebench/` (gitignored) is used when `<repo>/target` is a symlink, because the podman machine cannot follow a link to another volume to mount the run's `pcap/` folder. `--json-out` overrides. |

## Images

**nodebench-asl3.** ASL3 `asl3-asterisk 2:22.10.1+asl3-3.10.5-1.deb12`
(Asterisk 22.10.1).

- It is built from the `bookworm` `main` suite, which has arm64 and amd64
  packages.
- Stock `asterisk.conf` is kept because its `astmoddir` is arch-specific
  (`/usr/lib/aarch64-linux-gnu/...` on arm64).
- It was built and run on arm64 (the Mac VM). **amd64 is not yet verified**;
  the first run on the IONOS runner settles it.
- The fresh build takes about 20 s.
- No DAHDI, dkms, `asl3-update-nodelist`, allmon3 or asl3-menu is installed.
  Timing is `res_timing_timerfd`.

**nodebench-astar-server.** `cargo build --release --locked -p astar-server`
runs inside `rust:1-bookworm` for the VM's own arch, with no qemu.

- Cargo registry and target dirs are buildah cache mounts
  (`nodebench-cargo-*`).
- A fresh image takes about 47 s on the 10-core VM, of which 17 s is cargo.
- `RUSTUP_TOOLCHAIN` is pinned to the image's toolchain, so
  `rust-toolchain.toml` (`stable`) does not trigger a rustup download.

## Ports and credentials

The containers share one network namespace, and node-to-node traffic is
`127.0.0.1` both ways.

- **Inside the pod, sockets bind `0.0.0.0`.** Podman delivers host-forwarded
  traffic to the pod's interface, not to its loopback. A socket bound to
  `127.0.0.1` in the pod is unreachable from the host. This was tested: with
  `127.0.0.1` binds, `curl 127.0.0.1:8730` from the Mac never answered.
- **Every published port is on the host's `127.0.0.1`.** The pod interface
  (10.89.x on the Mac VM) is reachable only from inside the podman VM, or on
  Linux, from the runner box itself.

| Host 127.0.0.1 | Pod | Proto | What |
|---|---|---|---|
| 24569 | asl3 `0.0.0.0:4569` | UDP | ASL3 IAX2 |
| 25038 | asl3 `0.0.0.0:5038` | TCP | AMI |
| 24570 | astar-server `0.0.0.0:4570` | UDP | astar-server IAX2 listener (Asterisk owns 4569) |
| 28730 | astar-server `0.0.0.0:8730` | TCP | astar-server control HTTP + SSE |
| 32001 | usrp-relay `:32011` → `127.0.0.1:32001` | UDP | bench → node USRP (the radio's RX audio and PTT) |
| (outbound) | node → `127.0.0.1:34001` → usrp-relay → `host.containers.internal:34001` | UDP | node → bench USRP (what the node transmits) |

**Host ports are the services' own plus 20000** (changed 2026-10-11 by the
integrator; the gate below ran on the unshifted ports). Other tests on the
same Mac dial `127.0.0.1:4569` and `8730` (astar-iax fixtures with usernames
such as `mgr-test` and called numbers `55553`, `n1`, `42`), and with the rig
published there they landed in the pod: a storm of stray NEWs to the node in
the middle of a scenario, and an unexplained `unknown` codec. Inside the pod
nothing moved: the node is still `127.0.0.1:4569` and astar-server
`127.0.0.1:4570`, and a `POST /link` still names `127.0.0.1:4569`.

**On CI (nested podman),** set `--var USRP_HOST=10.0.2.2`: with slirp4netns
`allow_host_loopback=true`, that is the runner container's own 127.0.0.1
(verified in the stand-in, README.md "CI").

**Credentials.** These are test-only and in the repo on purpose: nothing
outside the VM can reach them.

| What | Value |
|---|---|
| AMI | user `nodebench`, secret `nodebench-loopback-only` |
| IAX `[1998]` on the node (registration, and astar-server's link username) | secret `nodebench-loopback-only`, `auth = md5` |
| astar-server env (pod.yaml) | `ALLSTAR_NODE=1998`, `ALLSTAR_SECRET` and `ALLSTAR_LINK_1999` = the same secret |

### What reaches the host, verified

- **UDP request and response, host → pod: works.** An IAX2 POKE from the Mac
  to 127.0.0.1:4569 and 4570 got a PONG in about 1 ms. Asterisk sees the
  host as source `10.89.2.x`, not 127.0.0.1, which matters for ACLs and for
  per-IP callno counts.
- **TCP (8730, 5038): works.** AMI login was accepted from the Mac.
- **Pod → host UDP, not as a reply: no published port can do it.** The relay
  uses `host.containers.internal`. On the Mac it is 192.168.127.254, which
  gvproxy maps to the Mac's 127.0.0.1. Verified: a datagram from the pod
  arrived at a Mac socket bound to 127.0.0.1, and the USRP loop below works.
- **On Linux (CI), `host.containers.internal` is the host's own address, not
  its loopback.** The bench binds 127.0.0.1 only, so the relay is pointed
  at the runner's loopback instead: `--var USRP_HOST=10.0.2.2` for
  `nodebench run`, or `NODEBENCH_USRP_HOST=10.0.2.2` for `rig.sh up`
  (slirp4netns with `allow_host_loopback=true`; verified in the nested
  stand-in, see above and README.md "CI").

### USRP and the host

The rxchannel is `USRP/127.0.0.1:34001:32001`, which reads as
`host:txport:rxport`.

- chan_usrp **binds `127.0.0.1:32001`**, and that is where it receives the
  radio's audio.
- It **sends its transmit audio to `127.0.0.1:34001`**.
- Both are on the pod's loopback, hence the `usrp-relay` sidecar (two
  `socat -u`).
- The host bench sends USRP frames to `127.0.0.1:32001` and receives the
  node's transmissions on `127.0.0.1:34001`.

**Verified from the Mac:**

- I sent 2 s of keyed 1 kHz slin (USRP voice frames, keyup=1, 160 samples
  each).
- The node read 101 frames and, at duplex=2, repeated them.
- The Mac received 315 keyed USRP TX frames, peak 8000, with first audio
  104 ms after the first frame sent.

**If the relay ever proves unreliable,** the fallback is to run the bench
binary inside the pod as one more container, where everything is
127.0.0.1. Build it in the same `rust:1-bookworm` stage. AMI covers what
`podman exec asterisk -rx` does.

## Variants (`rig.sh up` options)

| Option / env | Effect |
|---|---|
| (default) | Node codecs `slin16,slin,ulaw` (offer order). astar-server uses `node.toml`. |
| `--ulaw` / `NODEBENCH_CODECS=ulaw` | ulaw-only node, the ClearNode stand-in. Any comma list of ulaw, alaw, slin, slin16, gsm, adpcm also works. |
| `--debug` / `NODEBENCH_DEBUG=1` | `core set debug 2`, `iax2 set debug on` and `rpt debug level 7` at start. The full log is `rig.sh full-log`. |
| `--register` | astar-server uses `node-register.toml` and registers with the node. Before `18eed6b` this reproduced the registration storm below; now it registers with one call number. |
| `--fill-callerid` | Dialplan workaround (also switchable at run time: `dialplan set global NODEBENCH_FILL_CALLERID 1`): sets `CALLERID(num)=1998` on calls from IAX user 1998 that arrive without one. **Off by default**, so the default rig shows the bug. |
| `RUST_LOG` | astar-server log filter (default `debug`) |
| `NODEBENCH_CAPTURES` | pcap directory |
| `NODEBENCH_USRP_HOST` | Where the relay sends the node's USRP TX |

`rig.sh up` takes about 1 s: app_rpt has node 1999 and `/status` answers.

### Why `node.toml` points `[register]` at a dead port

astar-server's node id comes **only** from `[register].node_id`. It is also
the IAX USERNAME on link dials; with no `[register]`, links dial with an
empty username.

So the default config keeps `[register] node_id = "1998"`, but points
`peer` at `127.0.0.1:4599`, where nothing listens. Registering against the
real node used to trigger the storm below (finding 2, fixed), which
exhausted the node's call numbers for 127.0.0.1. `register_with_node`
overlays the real peer for its own run; the default stays off the node so
every other scenario starts from a node with no registration.

## Driving it

```bash
rig.sh cli 'rpt nodes 1999'            # T1998 when linked
rig.sh cli 'rpt lstats 1999'           # direction, connect time, state
rig.sh cli 'rpt cmd 1999 ilink 3 1998' # node dials astar-server (transceive)
rig.sh cli 'rpt cmd 1999 ilink 1 1998' # node drops it
rig.sh cli 'iax2 show channels'        # codec, seq, lag
rig.sh cli 'iax2 show callnumber usage'
curl -s 127.0.0.1:28730/status
curl -s -X POST 127.0.0.1:28730/link -d '{"action":"connect","node":"1999","addr":"127.0.0.1:4569"}'
curl -s -X POST 127.0.0.1:28730/link -d '{"action":"disconnect","node":"1999"}'
```

**A quirk for scenario authors:** a link app_rpt dials **in** to
astar-server appears in `/status` as an `Active` entry in `calls[]`, with
`node: ""`. It does not appear in `links[]`, which only holds links
astar-server dialled itself. `hold.sh` counts either.

## Gate results, 2026-10-11 (arm64 Mac VM)

Each gate is in `findings/gate-<variant>/`:

- `hold-*.txt`: per-second polls.
- `asterisk-full.log.gz`: iax2 debug and rpt debug 7.
- `astar-server.log`: `RUST_LOG=debug`.
- `iax-full-frames.pcapng.gz`: full frames only.
- `lstats-*`, `after-*-disconnect.txt`.

| Variant | node → server (`ilink 3`), 90 s | server → node (`POST /link`), 90 s |
|---|---|---|
| default (`slin16,slin,ulaw`) | **held 90 s**, negotiated slin; `ilink 1` clears both sides | **never up**: app_rpt hangs up in about 1 ms |
| `--ulaw` | **held 90 s**, negotiated ulaw | **never up**, same cause |
| `--fill-callerid` | held 90 s | **held 90 s**, negotiated slin16 |
| `--ulaw --fill-callerid` | held 90 s | **held 90 s**, ulaw |

**The 1–2 s drop did not reproduce in the node → server direction.** A link
app_rpt dials in to astar-server held 90 s with both codec sets, with no
drop.

**The server → node direction fails at once, every time, until the caller ID
is filled in.**

### Finding 1: astar-server's link NEW has no CALLING NUMBER, and app_rpt refuses it

**Who hung up:** ASL3 (app_rpt), 0.4 ms after its own ACCEPT, before any
ANSWER.

The last frames (from `findings/gate-default/asterisk-full.log.gz`, 20:42:45):

```
astar → NEW      USERNAME=1998 CALLED NUMBER=1999 FORMAT=slin16 CALLTOKEN(empty)   (no CALLING NUMBER IE)
ASL3  → CALLTOKEN
astar → NEW      (with token)
ASL3  → ACK, AUTHREQ (md5, USERNAME 1998)
astar → AUTHREP
ASL3  → ACK, ACCEPT  FORMAT=slin16
        app_rpt.c: Node  invalid for connection: Caller ID is not numeric
ASL3  → HANGUP   CAUSE CODE 0                      (+0.4 ms after ACCEPT)
astar → ACK, ACK
```

**The cause.** app_rpt takes the linking node's number from the channel's
caller ID. `link_dial_params` in `crates/astar-server/src/controller.rs`
passes `node_id` as `caller_id`. The station then puts it in the NEW as
USERNAME (`crates/astar-iax/src/runtime.rs`, `username: caller_id`) but
sends no CALLING NUMBER.

**The workaround proves it.** `--fill-callerid` sets `CALLERID(num)` in the
node's dialplan, and with it the same dial holds 90 s.

**What astar-server reports.** `POST /link` returns `{"ok":true}`, and the
link is gone from `/status` by the next poll. astar-server logs nothing
about the dial or the HANGUP at `RUST_LOG=debug`.

**This is a candidate for `iax-cnlink`, not a confirmed match.** It fits if
the ClearNode drops a link that astar-server dials, and app_rpt on any node
would do the same thing. Here the drop takes about 1 ms on loopback, not
1–2 s. A real ClearNode's dialplan, and an internet round trip, could
stretch that. Rob should confirm which side dials in his case.

### Finding 2: registration against ASL3 storms and exhausts the node's call numbers

This shows with `rig.sh up --register`. Artifacts are in
`findings/register-storm/`.

**What happens on the wire:**

- astar-server sends REGREQ (USERNAME 1998, REFRESH 60, empty CALLTOKEN)
  from an ephemeral port, not the 4570 listener.
- ASL3 answers CALLTOKEN.
- astar resends REGREQ with the token but `oseqno=2`, where the first one
  carried `oseqno=1`, and a retransmission of it carried 0.
- ASL3 answers **VNAK** and keeps the half-made call number.
- This repeats in bursts of about 8,000–20,000 frames every 10 s.

**What it does to the node:**

- Within 2 s `iax2 show callnumber usage` is at **2048/2048 for 127.0.0.1**.
- Asterisk logs `maxcallnumber limit of 2048 for 127.0.0.1 has been
  reached!`, and keeps sending PING/LAGRQ to the thousands of dead call
  numbers.
- `/status` stays `registered: false`.
- **No link works in either direction while the storm runs.** `ilink 3`
  fails, and astar's NEW gets REJECT ×7.

**Who is at fault:** astar-server's registration client, with call tokens.
The node behaves as stock Asterisk does. The first 200 and 300 frames are in
the pcaps.

### Finding 3: after `POST /link disconnect`, ASL3 keeps the link about 8 s

This was seen with `--fill-callerid`.

- astar-server sends HANGUP with **timestamp 0**, as is the RDUNKEY just
  before it.
- Asterisk logs `Received out of order packet ... ts = 0` and resyncs its
  jitter buffer.
- It logs "Immediately destroying ... having received hangup", but `rpt
  nodes 1999` still lists T1998 for 8 s afterwards. Asterisk keeps PINGing
  the dead call number.
- By contrast, the node-side `ilink 1` clears both sides within 3 s.

### Findings 4–8: the quick tier (`nodebench run`, 2026-10-11)

These came out of the scenarios in `crates/astar-nodebench`, each from the
tshark capture with frame-level evidence. Every one is astar-server
behaviour; the ASL3 node behaves as stock app_rpt does.

**4. astar-server's ACK is one sequence number behind, and it accepts
stale frames on a reused call number.** (`relink_after_drop`; this is the
best `iax-cnlink` candidate so far.)

- astar-server ACKs a frame with `iseqno` equal to that frame's `oseqno`.
  Asterisk ACKs with `oseqno + 1`, and only that acknowledges it. Mid-call
  this is hidden, because the next frame astar sends carries a correct
  `iseqno`. The last frame of a call (the node's HANGUP) is never
  acknowledged, so Asterisk retransmits it 2 s later.
- astar-server reuses IAX call number 1 for the next inbound call, and
  routes the retransmitted HANGUP (source call = the OLD Asterisk call) to the
  NEW call on that number. It tears the new link down, ACKs the HANGUP to the
  new call's number, and answers the node's further frames with INVAL.
- The node, meanwhile, still lists the link as ESTABLISHED.
- Repro: `ilink 3`, `ilink 1`, `ilink 3` within ~2 s. The second link is gone
  from astar-server 1.2–2.5 s after it came up, every time.
- The same off-by-one is why the IAX probe's last DTMF digit (`#`) is
  retransmitted, and astar-server counts the retransmission as a second `#`
  (`dtmf_link_command`): it does not drop duplicate full frames by `oseqno`.

**5. astar-server's call tokens can contain NUL bytes.** About 6 % of inbound
links fail at setup.

- astar-server issues a 16-byte random binary CALLTOKEN. Asterisk handles a
  call token as a C string, so it echoes the token truncated at the first NUL
  (a token starting with `00` comes back empty).
- astar-server then REJECTs the resent NEW with "Call Token Invalid", and
  app_rpt sits in "Connecting" until it gives up.
- Probability per call: 1 − (255/256)^16 ≈ 6 %. The scenarios name it when it
  happens ("NUL byte(s) at [8]; the node echoed 8 byte(s)").

**6. astar-server never answers app_rpt's `!NEWKEY1!`, and never sends
RADIO_KEY on a node-dialled link.** Nothing it sends is heard on the node
after the first 2 s of a link.

- app_rpt opens every link with a `!NEWKEY1!` TEXT frame. Without one back
  within `NEWKEYTIME` (2 s), it logs `newkeytimer expired on connected node`
  and from then on only treats the far end as keyed after
  `AST_CONTROL_RADIO_KEY`.
- astar-server streams voice to the node but sends no RADIO_KEY when a bridge
  member talks, so the node never transmits it (`audio_server_to_node`, and
  the server -> node half of `monitor_link`, both measured after the node's
  own "connected" telemetry has finished).

**7. astar-server bridges audio from a link that never keyed.** On an
`ilink 2` (monitor) link, app_rpt sends its audio but never RADIO_KEY, which is
how a monitor link stays one-way. astar-server mixed it to the bridge at full
level (`monitor_link`).

**8. Registration storm (finding 2) reproduces in `register_with_node`**:
about 2,000 REGREQs and VNAKs within 20 s, and the node's call numbers for
the pod address hit 2048/2048. The scenario restarts the node afterwards.

Everything else the quick tier checks passes: node-dialled links hold 60 s;
codec negotiation agrees across all 24 cells (wire ACCEPT, astar's snapshot
and the node); auth off/optional/required behave as configured, with
REJECT "No such user" and "Authentication failed" where they should;
keepalives are all answered; and node -> server audio arrives at 1000.0 Hz,
−10.3 dBFS, onset about 65 ms, no loss.

**A note on the node's `iax2 show channels` Format column.** It is the
format of the last voice frame received, so it reads `unknown` until the far
end sends audio. The codec matrix reads the negotiated codec from the ACCEPT
on the wire instead.

### Fix status (2026-10-11, `work/nodebench`)

- **Finding 1, fixed** (`3c34307`): node links send CALLING NUMBER = our node (`CallMode::NodeLink`). `server_links_to_node` holds 60 s with no `--fill-callerid`, in slin16 and against a ulaw-only node.
- **Finding 4, fixed** (`626df4d`): inbound legs count the opening/token NEW, so ACKs carry `oseqno + 1`. A frame from a stale peer call gets INVAL instead of reaching the reused call number. `relink_after_drop` passes 6 of 6 runs, and `dtmf_link_command` passes.
- **Finding 5, fixed** (`fb17eb8`): call tokens are printable.
- **Findings 2, 3, 6, 7 and 8, fixed on `work/iax-fixes` (2026-10-12):**
  - **2 and 8, registration storm** (`18eed6b`). The FSM's retry timers enqueued a fresh REGREQ under the next `oseqno` while `Reliability` was retransmitting the first, so after a late registrar the token REGREQ went out at `oseqno` 3. chan_iax2 opens a new call for every `dest_call = 0` token REGREQ and expects `oseqno` 0 there, so it VNAKed. The VNAK made astar resend everything in flight, each resend opened another call, and so on. `RegReqResent` also had no frame arm, so REGAUTH after the token round was dropped. Now the token REGREQ restarts numbering (`ResetReliability`, as chan_iax2's `resend_with_token` does), the retry timers only bound the wait, and a VNAK from a call we never learned is not answered. `register_with_node`: registered at once, refreshes, **1 call number** on the node (was 2048/2048), 0 VNAKs. Loopback: `crates/astar-iax/tests/register_calltoken_loopback.rs` (17 million call numbers in 3 s against the fake before the fix).
  - **6, NEWKEY** (`564d091`, `306bd92`). app_rpt sends its link strings as C strings, NUL included, and astar compared `!NEWKEY1!` with the NUL, so it never answered. (A frame whose `iseqno` acknowledges the ANSWER also reached the FSM before it went Active; fixed in the same commit.) Once NEWKEY was answered, app_rpt treated astar's voice frames as its key, and the bridge sent every member silence on every tick, which held the node's transmitter up for good. The bridge now sends a member the mix only while someone it hears is talking, plus 100 ms, the way app_rpt sends link voice only while keyed. Inbound legs stamp voice from the wall clock, re-anchored after a gap. `node_links_to_server` and `server_links_to_node` now check NEWKEY; `audio_server_to_node` passes (700 Hz, −11.3 dBFS, onset 116 ms, 0 % loss). Loopback: `app_rpt_link_text_loopback.rs`; conference unit tests.
  - **7, monitor bridging** (`dbeeeb6`). With NEWKEY answered, app_rpt sends no voice at all on its monitor link. The old failure needs a peer that did not do NEWKEY within 2 s: app_rpt then streams voice and marks its transmission with RADIO_KEY, which a monitor link never sends. Each leg now keeps app_rpt's rule (`link_keying`): a peer that sends app_rpt link TEXT but did no NEWKEY is heard only between RADIO_KEY and RADIO_UNKEY. Phones, astar clients and probes, which send no link TEXT, are always heard. `monitor_link` passes. The RADIO_KEY-mode half cannot be produced with ASL3 (it always does NEWKEY), so it is pinned by `link_keying_loopback.rs`.
  - **3, slow clear after `/link disconnect`** (`a77f81b`). Timestamp 0 was not the cause. app_rpt drops a link at once only when it has read `!!DISCONNECT!!` (its own `ilink 1` sends it); a link that just hangs up it holds for `DISC_TIME`, 10 s, expecting it back (`LINKDISC AA` in its log exactly 10.0 s after the HANGUP). And chan_iax2 discards a channel's queued frames once a HANGUP is processed, so the TEXT has to arrive first. A leg whose peer is app_rpt now sends `!!DISCONNECT!!` and holds its HANGUP up to 250 ms; the node normally hangs up first. Control, TEXT and HANGUP frames now carry the call clock instead of 0. `disconnect_from_each_side`: the server side clears in **0.22 s** (deadline now 2 s; was 10.2 s), and the dead-call PINGs are gone with it. `dtmf_link_command`'s drop went from 10 s to 0.10 s.
  - **Full quick tier after all of it:** run `20261013T025532Z` (60 s holds), 15 scenarios, **100 of 100 checks pass**.
  - **Follow-ups found by the full quick tier.** Once app_rpt's TEXT matched, astar's old `!!DISCONNECT!!` arm ended a leg on the TEXT, leaving the HANGUP app_rpt sends in the same millisecond unacknowledged, and the node retransmitted it (`relink_after_drop`); the leg now stays up until the HANGUP (`5dd480d`). The app_rpt link rules (`!!DISCONNECT!!` on hangup, the RADIO_KEY gate) are kept off web-transceiver calls, which are guests on a node, not links (`acdf8fd`).
  - **Token-less NEW retransmit** (`ca622a2`). A caller whose CALLTOKEN was lost retransmits its NEW without a token; the leg answered REJECT "Call Token Invalid". It now re-sends the CALLTOKEN, as chan_iax2 answers every token-less NEW. New scenario `calltoken_retransmit` plays the caller from the host; it fails on the old image and passes now.
  - **Debug noise** (`2506816`, `9292841`). All three lines were real, if harmless, state-machine gaps, and all three are gone. `listener: invalid_transition` 10 s into inbound links was the CALLTOKEN expiry timer, never cancelled on a valid token. `client: unhandled_subclass_in_active` at outbound setup was app_rpt's `ast_indicate(chan, -1)` after ANSWER: control byte 0xff, which chan_iax2 reads as −1 (astar decodes `1 << 31`). `registration: invalid_transition` about 15 s after start (the default config's dead registrar port) was `Reliability`'s give-up reaching a state with no arm for it.

### Other observations, for the scenario authors

- **Keepalives.** astar-server sends PING and LAGRQ every 2 s on every link,
  in both directions, and ASL3 answers each. ASL3 sends LAGRQ every 10 s and
  an occasional PING, and astar answers.
- **Retransmissions.** ASL3 retransmitted one LAGRP early on the inbound
  link in each gate; astar's ACK `iseqno` appeared one behind (finding 4,
  fixed). The only other retransmissions were two PINGs to the outbound
  link's call number after astar's HANGUP (finding 3, fixed).
- **Text frames.** app_rpt sends TEXT frames, the link list and keepalive,
  about every 30 s, and astar ACKs them.
- **Debug noise, fixed 2026-10-12.** astar logged
  `listener: reason="invalid_transition"` 10 s into an inbound link, and
  `client: reason="unhandled_subclass_in_active"` at outbound link setup.
  Neither dropped anything; both were state-machine gaps (see Fix status).
- **Codecs.**
  - On node → server, app_rpt asks for **slin**, not slin16, even with
    slin16 first. Its capability is 0x8044 = ulaw|slin|slin16.
  - On server → node, astar asks for slin16 and gets it.
  - The ulaw node gets G711U both ways.
- **Disconnect from the node side** (`ilink 1 1998`) is clean: both sides
  are empty within 3 s.

## Speech echo (2026-11-13, `work/link-proof`)

The tone scenarios prove audio crosses the link; speech is the harder test,
because it starts and stops. It exercises keying, the unkey at the end of an
over, and the first syllable. The bench sends one spoken phrase,
`fixtures/link-test-phrase.wav` (5.18 s, 8 kHz), and
`astar_nodebench::speech::compare` lines what comes back up against it: the
offset, the correlation of the two loudness curves, the level difference,
how much of the phrase's speech arrived (and of its first and last 200 ms),
and any gaps, repeats or stalls.

### `parrot_echoes_speech`

astar-server runs with `[bridge] mode = "parrot"`, `[parrot]
playback_delay_ms = 3000` and `silence_gap_ms = 800` (a server overlay). The
node links to it with `ilink 3`; the bench keys the phrase into the node over
USRP; astar-server records the over and plays it back down the link; the node
transmits the echo on USRP. It runs once against the normal node and once
against the ulaw-only node (a node overlay).

The echo is measured in two places:

- **On the link**, as astar-server sent it: the voice frames from 4570 to
  4569 in the capture, mini frames included, decoded
  (`pcap::voice_frames`). These are the strict checks, and they compare the
  echo with **the over as astar-server received it** (the frames from 4569
  to 4570), not with the phrase: the over arrives already shaped by
  app_rpt's input path (a trimmed first and last frame, now and then a
  20 ms stall or repeat under host load), and astar-server's job is to give
  back exactly what it took in. Both are also compared with the phrase, as
  notes.
- **At the node's radio**, as the bench recorded it on USRP after the over
  ended. The node repeats the over on its own transmitter while the bench
  keys it (it is a full-duplex repeater), so only what it sends after the
  over can be the echo. This copy has crossed app_rpt and the host relay
  twice, and each crossing can stall, repeat or trim a frame of its own, so
  here the echo must arrive mostly whole and at the right level; its
  correlation and its gaps, repeats and stalls are notes.

The timing checks use the wire: from the node's RADIO_UNKEY to
astar-server's first echo frame. `app_rpt` keys a link with voice alone and
never sends RADIO_KEY, so the parrot's PTT gate never latches and the take
ends on VOX: the echo is due 800 ms (the gap) + 3000 ms (the delay) after
the unkey.

**Thresholds and the runs they come from.** Three runs on the arm64 Mac VM
(`20261114T055655Z`, `055904Z`, `060120Z`, after fixes S1 and S3), each
against the normal node (slin) and the ulaw node, so six echoes:

| | Echo on the link, against the over | At the node's radio, against the phrase | Threshold |
|---|---|---|---|
| correlation | 1.000 in all six | 0.733–0.997 (note only) | link ≥ 0.95 |
| level | −0.29 to −0.30 dB | −0.29 to −0.32 dB | link ±1 dB, radio ±3 dB |
| speech present | 100 % in all six | 94.4–99.4 % | link ≥ 99 %, radio ≥ 90 % |
| first / last 200 ms | 100 / 100 % in all six | 67–100 / 88 % | link ≥ 80 %, radio ≥ 60 % (4 of the 6 speech frames) |
| gaps, repeats, stalls | none in all six | 20–60 ms in three of six (note only) | link: none |
| echo after the unkey | 3783–3898 ms (wire) | 3975–4235 ms (bench clock, note only) | > 0, and 3800 ms −250/+500 |

The over itself, against the phrase, measured correlation 0.748–1.000,
98.9–99.4 % present, head 83–100 %, tail 88 %, with 20–40 ms stalls or
repeats in three of six: that is app_rpt and the USRP path from the host,
before astar-server sees it, and the echo on the link reproduces each one
exactly. The first run of a pod on a loaded host (load average about 10)
was the worst at the radio (94.4 %, a 40 ms gap and a 60 ms repeat), with
its over already at correlation 0.748.

**Full quick tier after all of it:** run `20261114T060554Z` (60 s holds,
host load average 21–23 from other work on the Mac), 16 scenarios, **133 of
136 checks pass**. In `parrot_echoes_speech` every echo check passed in both
cells (echo against the over: correlation 1.000, 100 % present, level
−0.3 dB; delay 3825 and 3769 ms), and its two failures are S2. The third
failure was `audio_node_to_server`'s frame loss, 10 % (11 of 110 frames,
longest gap 7) under that load; it passed three reruns straight after, so
it is the host, not a finding.

Before fix S3, the earlier runs (`052422Z`, `052626Z`, `052853Z`, compared
with the phrase) had the echo on the link 97.2–99.4 % present, correlation
0.853–0.999, and one 20 ms stall or 40 ms repeat in each ulaw echo that the
over did not have.

### Findings

**S1. The parrot dropped the pauses inside a take (fixed, `2ed1dd9`).** In
VOX mode the parrot recorded only the frames above its threshold, so the
quiet frames between words were left out, and the echo came back with its
pauses squeezed out: the 5.18 s phrase returned as 3.72 s (186 frames on the
wire), correlation 0.28, 54 % present, "gap 2200 ms" at the end. A pause
shorter than the silence gap is now kept in place; the trailing silence that
ended the take is still dropped. Loopback test:
`conference::tests::parrot_vox_take_keeps_the_pauses_between_words` (it
failed first). After the fix the phrase returns at full length and timing.

**S2. The parrot hung up the link after every echo (fixed, `803adb3`).**
After replaying a take, the parrot queues its spoken signal report and then
hangs the leg up (`Manager`, "signal report complete"); with `[announce]`
off it hung up as soon as the replay had drained. On an AllStarLink link
that meant astar-server sent `!!DISCONNECT!!` 7 ms after its last echo
frame and the node dropped the link, so "link still up on both sides 5 s
after the echo" failed in every run. Rob ruled (2026-11-14) that a node
link stays up after its echo, so it can talk again, and every other caller
is still hung up after its report. The leg's run-loop now publishes what
`link_keying` already knew (`Call::is_app_rpt_peer`), and the report pump
leaves such a leg linked. Loopback test:
`an_app_rpt_node_link_stays_up_after_its_echo_and_echoes_again`
(`crates/astar-iax/tests/parrot_loopback.rs`, failed first). The scenario
now keys a second over on the same link and requires a second good echo.

**S3. A frame that missed its bridge tick left a 20 ms hole in the take
(fixed for the parrot, `90efb10`; for the bridge, see S6).** Each conference
member's input was drained once per 20 ms tick with nothing held back
(`Source::decode_into`, `crates/astar-audio/src/conference.rs`). When a
member's packets arrive close to the tick edge, a frame 3 ms late misses its
tick: that tick reads silence, and the late frame plays one tick later. The
parrot recorded the silent tick, so in run `052853Z` (ulaw), where the
node's over arrived paced within ±3.4 ms and compared clean (correlation
1.000), the take astar-server replayed had a 20 ms stall at 220 ms; in
`052626Z` it was a 40 ms repeat at 940 ms. The S1 fix made this more likely,
because it began recording quiet ticks. A take now records only the frames
the caller sent (in both the VOX and the PTT gate), so a missed tick adds
nothing. Loopback test:
`conference::tests::parrot_take_has_no_hole_where_a_frame_was_late` (it
failed first, with `[9000, 0, 8000, 7000]`). Since then every echo on the
link has matched its over exactly. Bridge mode read members the same way,
and `speech_server_to_node` measured the same hole there (S6, fixed).

**S4. The link log named the node when astar-server ended the link
(fixed, `0abdc7c`).** When the parrot hung up (S2), astar-server sent
`!!DISCONNECT!!` and the node answered with HANGUP inside the 250 ms
grace, and the link log read `ended_by=them cause="peer hung up"`. Ruling
(2026-11-14): a teardown astar-server started on the wire is `us`, with
its own cause; only a HANGUP that arrives while astar-server had sent
nothing toward ending the link is `them`. Tests:
`a_peer_hangup_during_our_held_hangup_is_ours` and
`a_peer_hangup_with_nothing_from_us_is_theirs`
(`crates/astar-iax/tests/link_log.rs`).

**S5. app_rpt's own parrot cannot echo over a link, so the design's second
scenario (`node_parrot_echoes_speech`) does not exist.** The plan: put the
stock node in parrot mode, have astar-server dial it, speak the phrase in
from the IAX probe, and hear the echo back at the probe. What app_rpt
(asl3-asterisk 22.10.1+asl3-3.10.5) actually does:

- `parrot = 2` in rpt.conf leaves parrot mode off (`rpt stats` says
  "Parrot Mode: DISABLED"): the value is read as a boolean, so it takes
  `parrot = yes` (or `1`) to turn it on, and then it is always on.
- Once on, it records whatever its own transmitter sends (the monitor
  channel, in `duplex = 2`), link audio included, from when the transmitter
  keys until `parrottime` ms after it unkeys.
- It plays the recording back as telemetry (`PARROT`), and telemetry of that
  kind joins the transmit-only conference (`RPT_TXCONF`): the playback goes
  out the node's own radio and is never sent to its links. In app_rpt's
  source, `rpt_telemetry.c` picks `RPT_CONF` only for ID, PLAYBACK,
  TEST_TONE and GPS telemetry.
- In the trial run (`20261114T050334Z`), the probe received nothing at all
  after its over. The node did play a parrot file
  (`/tmp/parrot_1999_0.slin`), but it was a recording of its own
  link-connect telemetry, made before the over; after the over, no playback
  came within 10 s.

So astar-server can be shown to send speech *to* a stock node (the node
transmits it), but not to hear the node's echo of it: app_rpt has no echo
that returns over a link.

**S6. The bridge put a 20–40 ms hole into its output (fixed, `f68a869`,
`37de502`, `338d37a`; `iax-confjb`).** This is S3's cause seen in bridge mode, by
`speech_server_to_node` (below): the probe's audio reaches astar-server
whole (against the phrase: correlation 1.000, 100 % present, in every cell),
but in about half the overs what the bridge sends the node has a 20 ms
stall, a 40 ms repeat or a 40 ms gap that the input did not. The probe's
path in from the host (podman's port forwarding) arrives with a few
milliseconds of jitter, and the conference reads each member once per 20 ms
tick with nothing held back, so a frame that misses its tick leaves a hole.
A real AllStarLink client over the internet has far more jitter than this.

The fix is a jitter cushion on each member's input
(`crates/astar-audio/src/dejitter.rs`; the reasoning is in
`docs/design/rx-jitter-buffer.md`). Frames that carry the sender's clock are
held two ticks (40 ms) and played one per tick by their timestamps; a lost
frame is one tick of silence in its place; a late one slips the schedule
once and deepens the hold, to at most 80 ms; each talk spurt lines up
afresh on its own first frame. Loopback test:
`conference::tests::bridge_output_has_no_stall_repeat_or_gap_under_arrival_jitter`
(frames 3–15 ms late, two pairs arriving together, one lost; it failed
first, with a stall at the third frame). The rig then found two more causes,
each fixed with a test that failed first:

- **The mixing thread lost ticks under load** (`37de502`). A tick that woke
  more than 20 ms late pushed the whole schedule back, so the bridge sent 49
  frames that second and the member's input fell a frame further behind,
  which the cushion then trimmed: run `20261114T085543Z` (host load 13) had
  the bridge's transit time climb from 44 to 111 ms in one over and then a
  60 ms gap. A late tick is now caught up; only a stall over 100 ms is given
  up (`conference::tests::a_late_tick_is_caught_up_not_lost`).
- **The link unkeyed before the over was out** (`338d37a`). When
  astar-server dials the node, `sync_link_keying` keys the link while the
  probe is keyed, and closed the link's TX on the probe's `RADIO_UNKEY`,
  which follows its last voice frame at once, while the cushion still held
  the last 40–60 ms of the over: run `094455Z` lost the last three frames
  (tail 75 %, a 40 ms gap). A member that was keyed now counts as talking
  while the bridge still holds audio of its over, for at most 200 ms after
  its key was last seen; a source that never keyed keys nothing (`a4beb67`;
  `a_link_stays_keyed_until_the_bridge_has_played_the_over`,
  `voice_without_a_key_never_keys_a_link_or_opens_a_peer`,
  `the_cushion_tail_after_an_unkey_is_bounded`). In the rig's pcaps the
  NEWKEY node sends `RADIO_UNKEY` and never `RADIO_KEY`, so it never keyed
  links through `sync_link_keying` before the cushion either.

**Full quick tier after the rulings:** run `20261114T081239Z` (60 s holds,
host load average 8–16), 17 scenarios, **215 of 218 checks pass**.
`parrot_echoes_speech` passed whole, both overs in both cells. The three
failures are all S6, in `speech_server_to_node`: the bridge's output to the
ulaw node had a 20 ms stall (astar-server dialling) and, the node dialling,
96.6 % present at correlation 0.948 against its input.

**Full quick tier after the S6 fix:** run `20261114T103900Z` (60 s holds,
host load average 3.5–4.4), 17 scenarios, **218 of 218 checks pass**.
After the review fixes (the key tail bounded to 200 ms and only for a
source that keyed, `a4beb67`): four runs of the speech and tone scenarios
plus `monitor_link` (`20261114T111840Z`, `112537Z`, `113236Z`, `113933Z`,
131 of 131 each; 16 bridge overs, last 200 ms on the link 100 % in all),
then the full quick tier, run `20261114T114641Z`, **218 of 218**.

### `parrot_echoes_speech`, second over (2026-11-14)

With S2 fixed the link stays up, and the scenario keys the phrase a second
time on it. Runs `20261114T073902Z` and `074747Z`, both nodes, passed
whole: every second echo on the link matched its over exactly (correlation 1.000, 100 % present, edges 100 %), the delay was
3791–3816 ms, and the link held 5 s after each echo, both overs. At the
radio the second echoes measured 96.6–99.4 % present, edges 83–100 / 88 %,
−0.3 to −0.5 dB. In the run before them (`072135Z`, host load average
above 50 from other work on the Mac) one first echo's radio copy dropped to
84.7 % present: the rig's own hops, since its over on the link already had
repeats and its echo on the link matched that over exactly.

### `speech_server_to_node` (2026-11-14)

The other direction, which app_rpt's parrot cannot give (S5). astar-server
runs its normal bridge. Four cells: the normal node and the ulaw-only node,
each with astar-server dialling the node (`POST /link`) and with the node
dialling astar-server (`ilink 3`). The IAX probe (node 1997) joins the
bridge and speaks the phrase; the bench records the node's USRP output.

- **On the link** (strict, the same thresholds as the parrot's): the voice
  astar-server sent the node, against the probe's voice as it reached
  astar-server, both from the capture. The probe's path in from the host
  has jitter of its own, so the bridge is held to its input; both against
  the phrase are notes. Slin16 on the wire (the links astar-server dials)
  travels little-endian; 8 kHz slin, big-endian.
- **At the node's radio**: it keys for the over, carries 90 % of the phrase
  within 3 dB with four of each edge's six speech frames (the same radio
  thresholds as the parrot's), sends an unkey packet after its last keyed
  frame, and the link and the probe's call are still up 5 s later.

Measured in runs `20261114T073902Z`, `074747Z` (radio and keying only: the
link check there still misread slin16), `075743Z` and `080424Z`:

| | Measured | Threshold |
|---|---|---|
| probe's voice reaching astar-server, against the phrase | correlation 1.000, 100 % present, every cell | (note) |
| bridge output against its input, clean overs (4 of 8) | correlation 0.999–1.000, 99.4–100 % present, level −0.3 to −0.5 dB | as the parrot's link checks |
| bridge output against its input, the other 4 | a 20 ms stall, a 40 ms repeat or gap; correlation 0.905–0.967, 96.6–98.9 % present | fails: S6 |
| node keys up after the probe keyed | 55–155 ms | a keyed frame at all |
| node's last keyed frame after the probe unkeyed | 3.3–4.0 s (2 s hang time, then the courtesy tone) | an unkey packet after it |
| radio copy | 96.6–100 % present, head 50–100 %, tail 88–100 %, −0.3 to −0.8 dB | ≥ 90 %, edges ≥ 60 %, ±3 dB |
| link and probe up 5 s after the over | every cell | held |

The radio's head at 50 % came with an S6 gap at the very start of the
bridge's output; the radio copy carries the bridge's holes as well as the
node's.

**After the S6 fix** (2026-11-14), five runs with 10 s holds
(`20261114T100626Z`, `101253Z`, `101919Z`, `102546Z`, `103211Z`, host load
average 4–6), each with all four cells, so 20 bridge overs. All 126 checks
of the four audio scenarios passed in every run, the parrot's 20 echoes
included:

| | Measured, 20 overs | Before |
|---|---|---|
| bridge output against its input | correlation 0.999–1.000, 100 % present, first and last 200 ms 100 %, level −0.3 to −0.5 dB, **no gap, repeat or stall in any over** | a hole in 4 of 8 overs |
| the bridge's transit time, probe frame in to node frame out (pcap, ulaw cells) | 43–64 ms | 1–31 ms |
| node keys up after the probe keyed | 101–147 ms | 55–155 ms |
| node's last keyed frame after the probe unkeyed | 3.4–3.7 s | 3.3–4.0 s |
| radio copy | 97.7–100 % present, head 83–100 %, tail 100 %, −0.3 to −0.5 dB | 96.6–100 %, head 50–100 %, tail 88–100 % |

The cushion's cost shows where expected: about 40 ms more transit through
the bridge, and the tone scenarios' onset (key-up to first tone frame) moved
up by about as much: `audio_node_to_server` 107–148 ms in these runs against
58–91 ms in the earlier quick-tier runs (21 and 134 ms at the extremes), and
`audio_server_to_node` 138–167 ms against 125–201 ms, where the node's own
path dominates. Both stay far inside the 500 ms threshold, which is
unchanged.

## Safety

- **No traffic leaves the rig.** All node-to-node traffic is 127.0.0.1
  inside one pod. Published ports are on the host's 127.0.0.1 only. The
  relay's only outbound target is the host itself.
- **AllStar DNS is never queried.** `node_lookup_method = file`, and the
  `[nodes]` stanza names both nodes.
- **No HTTP registration.** `rpt_http_registrations.conf` has an empty
  `[registrations]`.
- **No statpost.** No `statpost_url` is set, and app_rpt 3.10.5 has no
  default URL.
- **No outbound HTTP from the dialplan.** `func_curl` is not loaded, and the
  stock `[allstar-public]` (authwebphone.pl) and `[my-ip]` (myip.vg)
  contexts are gone.
- **No node list download.** `asl3-update-nodelist` is not installed.
- **Not enforced at the network level.** The pod sits on podman's default
  network and *has* a default route. Its silence is configuration plus the
  capture: every UDP packet on `lo` and `eth0` goes to `captures/`.
- **The installs are documented exceptions.** The image build fetches
  packages from repo.allstarlink.org and deb.debian.org. That is package
  installation, not registration.
- `key_refusal` and the transmit time-out are untouched by the bench and
  by every fix above.
