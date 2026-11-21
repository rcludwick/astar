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

## SSTV: the outside decoder (2026-11-19, `work/sstv-tx`)

Later SSTV checks need a decoder that shares no code with astar, so a picture
astar sends is read by something that cannot agree with it by accident. The
image `localhost/nodebench-sstv-decode` holds two unmodified Python decoders,
each pinned to a commit (`images/sstv-decode/Containerfile`). `rig.sh build`
builds it, which needs the network once; it runs with `--network none` and
`decode <in.wav> <out.png>` prints `Detected SSTV mode <name>`. In Rust,
`astar_nodebench::outside::outside_decode(wav)` does that and returns the mode,
size and RGB pixels.

| Decoder | Modes it reads |
|---|---|
| `colaclanth/sstv` (tried first) | Martin 1/2, Scottie 1/2/DX, Robot 36, Robot 72 |
| `ta2ldv/sstv-decoder` (fallback when the first stops) | Martin 1/2, Scottie 1/2/DX, PD50, PD90, PD120; not Robot |

The first has no PD modes: on a PD120 recording it stops at the VIS code (the
number that names the mode), and `decode.sh` then hands the recording to the
second. The second has no licence file; it is fetched when the image is built
and never copied into the repo. The original C `slowrx` was not tried: it is a
GTK program, and running it headless means editing it, which would stop it
being unmodified.

Mean absolute error per colour channel (0 to 255) against the test picture
that `astar-sstv`'s tests use, from a 16-bit mono WAV made by the `sstv` crate
encoder:

| Mode | Decoder | Size read | 8 kHz | 44.1 kHz |
|---|---|---|---|---|
| Robot 36 | colaclanth | 320x240 | 32.8 | 24.9 |
| Scottie 2 | colaclanth | 320x256 | 27.2 | 27.3 |
| PD120 | ta2ldv | 640x494 | 2.33 | 2.27 |

The Robot 36 and Scottie 2 figures are large because the first decoder maps
tones to brightness its own way: the pictures look right (colour bars in the
right places, a grey ramp), but the bars come out darker than 191 and the ramp
reaches white early. So use these figures as a ceiling for "the decoder read
the picture", and compare with astar's own decoder for fidelity. PD120 reads
back at 494 rows rather than 496; the error above is measured at that size.

The test `outside_decodes_robot36` (in `outside.rs`) is ignored unless
`NODEBENCH_OUTSIDE=1`, because it needs podman and the image. It asserts the
mode, 320x240, and a mean error under 49.2 (32.8 and half again).

## SSTV through the node (2026-11-19, `work/sstv-tx`)

`sstv_station_to_node` is the proof the SSTV sender was built for: an astar
station sends a picture to a stock ASL3 node over a jittery network, and
something that shares no code with astar reads the picture off the node's
radio.

### What the scenario does

For each relay profile (`clean`, `net-1118`, `harsh`; seed 1118 every time):

1. A **headless station** starts. It is a `Station` built with `sstv-tx`
   whose audio backend is `NullBackend`: no sound device, and a microphone
   that never produces a sample, so the only audio on the call is the
   picture. (`PullBackend` cannot be used: it has no input device, and a
   call's connect refuses a backend with no default input.)
2. A **jitter relay** (`relay.rs`) starts on 127.0.0.1, aimed at the node's
   published IAX2 port, 127.0.0.1:24569. The station dials the relay's
   address, so every packet it sends crosses the relay.
3. The node lists the link as `1997`; the bench waits for the node to finish
   announcing the new link on the air.
4. The bench starts recording the node's radio (USRP), and the station calls
   `send_picture` with the test picture (`picture.rs`: colour bars, a grey
   ramp and a diagonal). The picture starts with 4 s of silence
   (`PICTURE_LEAD`; it was 0.3 s for the first measurement, see below).
   Robot 36 always; with `--long`, Scottie 2 and PD120 as well, the PD120
   station with its transmit time-out set to 3 minutes.
5. When the picture ends `Sent` and the node has been quiet for 1.5 s, the
   station hangs up and the recording is judged.

**How the station gets onto the node.** The station dials the way the app
dials a node: the Web Transceiver call shape, IAX2 username
`allstar-public`, `CALLING_NUMBER` = the node it wants (1999),
`CALLING_NAME` = who it is (1997). The engine has no other primary-call
shape, so the node's `[radio]` user (which needs username `radio`) cannot
take it. The image gained two pieces of configuration for it:

- `iax.conf`: an `[allstar-public]` user, md5 with the rig's test secret,
  context `nodebench-station`.
- `extensions.conf`: `[nodebench-station]`. Stock ASL3 runs
  `authwebphone.pl` here, which asks the AllStar portal who the caller is;
  the bench never reaches the portal, so it accepts the name `1997` and
  nothing else. It then sets the caller number to `1997` and runs
  `Rpt(1999,X)`, as the stock context does (`X`: an endpoint the dialplan
  has already checked). Without setting the caller number, app_rpt reads
  the dialled node as the caller and refuses with "Trying to link to
  self?".

### Three ways of looking at each recording

- **The decoders.** `astar-sstv` (slowrx, re-timing on, as the app decodes)
  is judged on the plain mean error per colour channel against the picture
  sent. The outside decoder is judged on its *fitted* error
  (`picture::fitted_error`): per channel, the line `decoded ≈ gain ×
  original + offset` is fitted first, and what is left over is measured.
  That forgives its own brightness scale (bars darker than 191, the ramp
  white early), which says nothing about whether the picture arrived. Its
  raw error is in the check's detail too. The gain is held at zero or
  above, so an inverted picture is not forgiven. When it reads fewer rows
  than were sent (ta2ldv has read PD120 as 640×494), the two are compared
  over the rows it read, from the top: rows 0–493.
  The third decoder is the `sstv` fork's own (`vendor/sstv`), with its
  de-jitter in front (`Decoder::decode_dejittered`), judged like slowrx on
  the plain mean error and required on every profile; see "The fork's own
  decoder" below.
- **What reached the node** (from the pod's capture, before the node does
  anything with it): how late each voice frame arrived against its own
  timestamp, which timestamps never arrived (the relay's drops), and whether
  the audio is the station's audio exactly.
- **The node's audio against the sent audio** (`timeline`): the recording
  is lined up against the station's exact audio 20 ms at a time, and every
  place where audio was dropped, made up, or came out silent is listed with
  its time in the picture. USRP sequence numbers only show frames lost
  between the node and the bench, and in every run here that was zero; this
  is what shows what the node's own jitter buffer did. A saved recording can
  be lined up again with `NODEBENCH_SSTV_WAV=<cell>.wav cargo test -p
  astar-nodebench --lib saved_recording -- --ignored --nocapture`
  (`NODEBENCH_SSTV_MODE=scottie2|pd120` for the long modes).
  A recording that comes back half a sample late (after a slin16 hop) is
  lined up against the sent audio delayed to match (`line_up`; see "SSTV
  through astar-server's bridge").

### The limits

Each limit in `scenario/sstv.rs` is twice the worst `clean` figure with the
4 s lead, among the `clean` cells where the line-up shows the node carried
the audio whole: nothing dropped, made up, silent or unmatched. A limit
backed by fewer than five such cells is marked provisional in the code.

| Mode | slowrx (plain) | outside (fitted) | fork, de-jittered (plain) | From |
|---|---|---|---|---|
| Robot 36 | 11.5 | 43.1 | 24.9 (provisional) | 7 slip-free `clean` cells of 12: worst 5.77 and 21.54 (29.42 raw). Fork: 4 slip-free cells of 8, worst 12.47 |
| Scottie 2 | 16.9 (provisional) | 32.1 (provisional) | 13.6 (provisional) | 1 slip-free `clean` cell of 2: 8.43 and 16.05 (28.16 raw). Fork: 2 of 2, worst 6.81 |
| PD120 | 12.9 (provisional) | 4.7 (provisional) | 22.1 (provisional) | 2 slip-free `clean` cells of 2: 6.46 and 2.33, read at 640×496 both times. Fork: 2 of 2, worst 11.06 |

The fork's cells come from both SSTV scenarios (`sstv_station_to_node` and
`sstv_station_via_server`, 2026-11-20); see the next section. Of its eight
`clean` Robot 36 cells, four were left out of the limit because the node
slipped in them: both cells of `091827Z` (the node dropped 10 ms and made up
40 ms; made up 40 ms in the two-hop one), `092514Z` (made up 20 ms at
8.84 s) and `093518Z` (made up 20 ms at 13.42 s). The four kept are
`092836Z`, `093156Z` and both cells of `093855Z`. Its error barely moved
with a slip (12.28–12.47 in the four left out), which is the de-jitter at
work.

A wrong or garbled picture scores about 85–100, so a limit set from cells
where the node slipped (78 or 89) would pass garbage; these do not.

A `clean` cell where the node did slip is judged on the same limit. It
fails if it is over, and is neither retried nor excused. The check's detail
then says what the line-up measured: what the node dropped or made up and
where, how many frames came out silent, and how late frames reached the
node. In the 12 runs at 4 s, that made **a `clean` Robot 36 cell red in 2 of
12 runs** (slowrx 39.06 after an 80 ms jump in `20261120T065809Z`; outside
44.70 after a 20 ms slip in `20261120T062313Z`) and the **`clean` Scottie 2
outside check red in 1 of 2** (42.67 after a 20 ms slip). A red `clean`
SSTV cell whose detail names a node slip is that, not a sender fault.

On `net-1118` and `harsh` slowrx must be under its limit; the outside
decoder must be only when the recording is whole. Otherwise its check is a
skip, "not required", whose text names what was measured (for example
"net-1118: the node dropped 40 ms, 2 frames silent") and gives its figure.

### The fork's own decoder (2026-11-20)

When the de-jitter (astar-sstv's re-timer) moved into the `sstv` fork, the
fork's own decoder with the de-jitter in front became the third decoder in
`judge`. It shares the de-jitter with slowrx's path, but not slowrx's
demodulator.

Runs: `20261120T091827Z` (both scenarios), `092514Z`, `092836Z`, `093156Z`
and `093518Z` (`sstv_station_to_node`), and `093855Z` (both scenarios,
`--long`): eight Robot 36 cells per profile, two each of Scottie 2 and
PD120. The checks ran with no limit first; every figure below is under the
limits pinned from them.

| Profile | Robot 36: fork (slowrx) | Scottie 2: fork (slowrx) | PD120: fork (slowrx) |
|---|---|---|---|
| `clean` | 11.45–12.47 (5.47–6.48) | 6.69, 6.81 (8.10, 9.96) | 10.87, 11.06 (4.21, 5.34) |
| `net-1118` | 11.60–13.16 (5.73–6.79) | 6.61, 7.42 (8.69, 10.09) | 10.77, 11.88 (4.87, 5.14) |
| `harsh` | 16.51–20.34 (5.56–6.52) | 7.39, 8.38 (9.71, 10.52) | 11.63, 17.26 (6.73, 8.75) |

- **It gives the picture back on every profile.** Every cell, in both
  scenarios, read the right mode and the full picture, and under jitter it
  stays close to its own `clean` figure, as slowrx does: the de-jitter does
  for the fork's decoder what it does for slowrx.
- **Its floor is its demodulator, not the timing.** On `clean` Robot 36 it
  sits at 12.47 in run after run, and PD120 at about 11, twice slowrx's.
  That is the 8 kHz noise the fork measurement found (defect 1 in the notes'
  "Measuring the `sstv` fork": a zero-crossing frequency counter with about
  two samples a pixel), and no re-timing can remove it. Scottie 2, whose
  pixels are longest, is the one mode where the fork beats slowrx (6.7
  against 8.1–10.0).
- **`harsh` Robot 36 costs it most** (16.5–20.3 against 12.5 on `clean`):
  the rows the de-jitter rebuilds are blended from noisier neighbours. The
  highest, 20.34, is under its limit of 24.9.

**Confirmed with the limits pinned** (run `20261120T110534Z`, both scenarios
once, Robot 36): 60 checks, 56 passed, 0 failed, 4 skipped (the outside
decoder off `clean`, as always). The fork, against its limit of 24.9:

| Profile | `sstv_station_to_node`: fork (slowrx; outside) | `sstv_station_via_server`: fork (slowrx; outside) |
|---|---|---|
| `clean` | 12.47 (5.47; 21.54) | 11.45 (5.53; 21.86) |
| `net-1118` | 11.75 (6.15; skip, 24.29) | 12.90 (5.99; skip, 48.15) |
| `harsh` | 13.27 (6.22; skip, 61.28) | 15.46 (8.81; skip, 60.38) |

Both `clean` cells were slip-free and match the cells the limit came from.

### The level

Every recording peaks at 16764, −5.8 dBFS, against the sender's −6 dBFS: the
node neither clipped nor compressed the picture. The clean slowrx error
through the node (5.6–6.3) is the same as through a bare loopback peer in
`astar-station`'s own test (5.5).

### First results, with a 0.3 s lead: 17 Robot 36 runs per profile, 1 `--long` run

All runs on the arm64 Mac VM, 2026-11-19 (UTC 03:15–04:29). Each run sends
one picture per profile. The station sent every picture in full, ended
`Sent`, 38.4 s for Robot 36, and the node listed the link within 0.9 s every
time.

| Profile | slowrx | outside | Relay drops; delay p50 / p99 / max (ms) | Lateness at the node, p99 / max (ms; the last 9 runs) | USRP frames lost |
|---|---|---|---|---|---|
| `clean` | 16 of 17 pass: 5.58–6.34, one 40.83 | 14 of 17 under 44.3: 21.54–31.19; one 54.37, two with no picture read | 0; 0 / 0 / 0 | 7–13 / 10–82 | 0 in every run |
| `net-1118` | 0 of 17: no picture started | 0 of 17: no VIS read | 5–6; 2 / 72–78 / 126 | 81–88 / 135–143 | 0 |
| `harsh` | 4 of 17 pass: 6.62–7.08; one 26.46; twelve with no picture started | never under 44.3: 56.8–58.6 in the five runs it read a picture; no VIS read in twelve | 25–27; 6 / 185–194 / 252 | 185–203 / 255–266 | 0 |

`--long` (run `20261120T040101Z`): on `clean`, Scottie 2 gave 8.75 (slowrx)
and 16.05 fitted (outside), and PD120 5.97 and 2.38 fitted, read at the full
640×496. On `net-1118` and `harsh` no decoder started a picture in either
mode.

The relay's delay figures are effective delays: a packet the schedule holds
back also holds back the packets behind it, since the relay keeps order. So
the p99 is several times the schedule's own (25 ms at `net-1118`).

### Finding, with the 0.3 s lead: the node's jitter buffer cuts 20 ms out of the VIS header

**On `net-1118` no picture starts, in any mode, in any run.** The cause is
the same every time, and it is in the node, not the network:

- **What reached the node was whole.** The capture shows the station's
  audio sample for sample, except the relay's six dropped frames, at 0.84,
  4.70, 15.64, 25.40, 28.20 and 34.56 s into the picture. None is in the
  header. Frames arrived up to 135–143 ms late.
- **The node's jitter buffer grows, then shrinks.** Early in the call the
  late frames make Asterisk's jitter buffer grow (in some runs the line-up
  shows 20–30 ms made up at about 0.35 s). It then shrinks again one 20 ms
  frame at a time, dropping a frame at 1.38, 1.92 and 2.56 s, and in some
  runs 3.22 s: the same times in every run, because the relay's schedule is
  seeded.
- **1.92 s is inside the VIS code.** The VIS code, the eight bits that name
  the mode, runs from 1.71 to 2.01 s of the transmission, 30 ms a bit. In
  the recording, bits 4–6 last 70 ms instead of 90 (the tone track of run
  `20261120T035741Z`: `1250` Hz for seven 10 ms steps where nine were
  sent). colaclanth reports "invalid parity bit", ta2ldv reads VIS 68 with a
  parity error, and slowrx never starts a picture.

`harsh` shows the same shrinking (20 ms dropped at 1.80, 2.46, 3.14 and
3.80 s), and its 400 ms blackout at 10.0 s comes out as about twenty silent
frames. Whether a drop lands inside the VIS bits depends on a few
milliseconds of timing, so 4 of 17 harsh runs started a picture, and those
slowrx decoded well (6.6–7.1, 33–39 rows rebuilt by its re-timing). The
outside decoder, with no repair, was never under its limit on `harsh`.

**`clean` is not quite clean.** The relay adds nothing, but the Mac's path
into the podman VM does: frames reached the node up to 82 ms late in one
run. The node's jitter buffer then slips on `clean` too. In run
`20261120T032938Z` it dropped 60 ms at 3.32 s, and slowrx came back at
40.83; in two runs a slip at 1.51 or 1.70 s cost the outside decoder the
VIS. So 1 slowrx and 3 outside `clean` cells of 17 failed limits set from
the first five runs. On a Linux host, without gvproxy, this may not happen.

What this meant for the sender: the node settles within about 3.4 s of
key-up, and with a 0.3 s lead the VIS started 1.7 s in (0.3 s of silence,
0.8 s of tuning tones, the leader). Any jitter-buffer slip in the 0.3 s of
VIS bits loses the whole picture at every decoder. So the lead was tried
longer.

### The lead-in experiment (2026-11-20)

`PICTURE_LEAD` was set by hand to 1, 2, 3 and 4 s, five scenario runs each
(Robot 36, all three profiles). With a lead of L seconds the VIS bits run
from L + 1.41 to L + 1.71 s after key-up. The 0.3 s row is the 17 runs
above. "Outside" counts runs under the `clean` limit of the time (44.3
fitted); off `clean` the check itself was a skip.

| Lead | `net-1118` slowrx / outside | `harsh` slowrx / outside | `clean` slowrx / outside | A node drop inside the VIS (`net-1118` / `harsh`) |
|---|---|---|---|---|
| 0.3 s | 0/17 / 0 (no VIS read) | 4/17 / 0 | 16/17 / 14/17 | 17/17 / not counted¹ |
| 1 s | 0/5 / 0 | 0/5 / 0 | 5/5 / 5/5 | 5/5 / 5/5 |
| 2 s | 5/5 / 2 of 3 read | 4/5 / 0 | 4/5 / 4/5 | 0/5 / 2/5 |
| 3 s | 5/5 / 5/5 | 5/5 / 0 | 5/5 / 5/5 | 0/5 / 1/5 (decoded anyway) |
| 4 s | 5/5 / 4/5 | 5/5 / 0 | 5/5 / 5/5 | 0/5 / 0/5 |

¹ Node drops inside the VIS were not counted for the 0.3 s `harsh` runs,
because the line-up that measures them was added later. What is known:
in 12 of the 17 runs no decoder read a VIS and no picture started; the
`harsh` drops the line-up did see came at about 1.80, 2.46, 3.14 and
3.80 s after key-up; and the VIS bits run from 1.71 to 2.01 s.

- **The drops come at fixed times after key-up, not after the audio.** The
  drop near 2.6 s is at 2.54–2.62 s at every lead where there is audio to
  see it, and the one near 3.3 s at 3.22–3.38 s. Drops inside the silent
  lead cannot be seen; they show only as a 10–50 ms offset where the tones
  begin. A longer lead does not stop the settling; it moves the header past
  it.
- **At 1 s** slowrx starts a picture but gets it wrong (error about 101,
  colours swapped): the 2.62 s drop cuts the last VIS bits and the start
  of the picture.
- **From 3 s** the header never met a settling drop on `net-1118`.

The ruling was a 4 s lead (`PICTURE_LEAD`,
`crates/astar-station/src/sstv_tx.rs`).

### Results with the 4 s lead (2026-11-20)

Ten scenario runs and two `--long` runs, 06:00–07:15 UTC, so 12 Robot 36
pictures per profile and 2 each of Scottie 2 and PD120. Every picture was
sent in full and ended `Sent` (Robot 36 42.1 s); USRP lost no frame in any
of them; no node drop fell inside a VIS header.

| Profile | slowrx | rows repaired | outside (fitted) | Relay drops; delay p50 / p99 / max (ms) | Lateness at the node, max (ms) |
|---|---|---|---|---|---|
| `clean` | 12/12 started; 5.47–5.77, one 39.06 | 0–7 | VIS read 12/12: 21.54–44.70 | 0; 0 / 0 / 0 | 13–111 |
| `net-1118` | 12/12: 6.17–6.89 | 0–10 | VIS read 12/12: 21.95–55.60 | 5–6; 2 / 66–72 / 126 | 134–143 |
| `harsh` | 12/12: 5.45–7.14 | 24–37 | VIS read 8/12: 54.2–61.3 | 25–27; 6 / 175–187 / 252 | 262–269 |
| `--long` Scottie 2 (clean / net / harsh) | 8.10, 8.43 / 8.94, 9.69 / 11.25, 10.36 | 0 / 0–3 / 26–33 | 42.67, 16.05 / 17.58, 16.39 / 53.95, 55.32 | | |
| `--long` PD120 (clean / net / harsh) | 6.46, 6.46 / 6.70, 6.58 / 5.86, 6.25 | 0 / 18–22 / 82–98 | 2.33, 2.33 / 33.69, 67.34 / 72.20, 71.38 | | |

**Slips inside the picture.**

- On `net-1118` the node still slips later in the picture: up to three
  times per Robot 36 picture, every one ±20 ms, at times such as 6.6–7.2,
  10.2, 16.7–16.9, 18.6, 24.8–25.5 and 34.6 s after key-up (5 of 12
  pictures had none). PD120, three times as long, had seven or eight, with
  two jumps of +80 and +100 ms in one run.
- On `harsh` there are 9 to 12 per Robot 36 picture, plus the 400 ms
  blackout at about 10.1 s, which comes out as silent frames.
- slowrx's re-timing repairs them: every `net-1118` and `harsh` picture
  passed, with up to 10 rows rebuilt on `net-1118` and 24–37 on `harsh`
  (82–98 on a `harsh` PD120). The outside decoder has no repair, and its
  fitted error rises with each slip (55.6 at worst on `net-1118`, 54–72 on
  `harsh`), so off `clean` it stays a recorded figure, not a pass condition.

**What still fails, and how often.**

- Against the limits above (slip-free `clean` figures), slowrx passed every
  `net-1118` and `harsh` picture. On `clean`, 2 of 12 Robot 36 runs and 1 of
  2 Scottie 2 runs had a red cell, each where the node slipped (see "The
  limits").
- **The node slips even on `clean`: in 5 of 12 Robot 36 pictures** and 1 of
  2 Scottie 2 pictures, although the relay adds nothing there.
  - In three of the Robot 36 ones a frame reached the node 47–111 ms late.
    That lateness comes from before the node: the Mac, its path into the
    podman VM (gvproxy), or the sender's own timing; the capture cannot
    tell those apart.
  - In the other two (`20261120T061141Z`, `20261120T062313Z`) the node made
    up 20 ms of silence while no frame was later than 17 ms. **The cause of
    those two is not established.**
  - *An unverified guess:* the sender's clock (the Mac) and the node's
    clock (the podman VM) run free of each other, and a jitter buffer has to
    slip now and then to absorb any difference in rate. What would settle
    it: over one long send (several minutes), measure the offset between
    the two clocks from the capture (arrival time against the sender's
    timestamp, fitted as a straight line) and compare the slip rate with
    the drift that line implies.
- **An 80 ms jump is more than slowrx's re-timer repairs.** In
  `20261120T065809Z` (`clean` Robot 36) the node dropped 80 ms at 7.7 s,
  and slowrx came back at 39.06 with 7 rows repaired, where a dozen 20 ms
  slips on `harsh` cost it nothing (5.5–7.1). Its WAV and decoded PNGs are
  in `.nodebench/20261120T065809Z/sstv_station_to_node/` (local, not in
  git). Backlog: `astar-sstv-retime-80ms`.
- The outside decoder failed to read the VIS in 4 of 12 `harsh` pictures.
  In each of those four, and in none of the eight it read, the node dropped
  20 ms at 5.38 s: in the second leader tone, 30 ms before the VIS bits
  start at 5.41 s. slowrx started all four pictures anyway. On `harsh` the
  jitter buffer is still moving about 1.4 s after the tones begin, so even a
  4 s lead does not keep the whole header clear there.

## SSTV through astar-server's bridge (2026-11-20, `work/sstv-tx`)

`sstv_station_via_server` is the two-hop version of `sstv_station_to_node`.
The pictures damaged on 2026-11-18 crossed a node in the middle of the
path, so this puts one there: astar-server.

### What the scenario does

For each relay profile (`clean`, `net-1118`, `harsh`; seed 1118):

1. astar-server links to the node (`POST /link`, transceive), and the bench
   waits until both sides list the link.
2. A jitter relay starts on 127.0.0.1, aimed this time at **astar-server's**
   published IAX2 port, 127.0.0.1:24570.
3. The same headless station as `sstv_station_to_node` dials the relay,
   asking for astar-server's node number (1998) and presenting itself as
   1997, as the IAX probe does (`common::iax_probe`). It still uses the Web
   Transceiver call shape (username `allstar-public`), the only primary-call
   shape the engine has. **astar-server's bench config needed no change for
   it:** its listener runs `auth = "off"` with no `allowed_nodes`, so it
   answers any caller, and it is only reachable from the host's 127.0.0.1.
   The bench waits until astar-server lists one new active call.
4. The station sends the picture (4 s lead, as before). astar-server's
   bridge mixes the call into the link; the node puts the link's audio on
   its radio, where the bench records it over USRP.
5. The recording is judged by the same `judge`, on the **same limits** as
   `sstv_station_to_node`. No two-hop limits were needed: the `clean` figures
   here (below) sit inside the existing ones.

Besides "the node's audio against the sent audio", each cell measures both
hops from the pod's capture, with the same lateness and line-up:

- **What reached astar-server**: the relay's output, voice frames to port
  4570.
- **What astar-server sent the node**: voice frames to port 4569 from
  astar-server's link socket. The link runs slin16, so those frames are
  16 kHz; every other sample is kept for the line-up.

A cell that fails keeps the pod's full frames (`<cell>.frames.txt`, via
`common::save_cell_frames`), the node's recording, and each hop's audio as
`<cell>.to-server.wav` and `<cell>.to-node.wav`.

### The recording comes back half a sample late

The first runs reported that the node "dropped 303 ms at 5.87 s" on `clean`,
with 1,300 of about 1,800 frames unmatched, while slowrx decoded the picture
at 5.7. Nothing had been lost. The picture crosses the link as slin16, so
it is resampled 8 → 16 → 8 kHz on the way, and it comes back:

- **half a sample (62.5 µs) late**: a fractional-delay search against the
  sent audio peaks at −0.50 samples, correlation 0.999, all through the
  picture;
- **gently rolled off**: −0.5 dB at 1–1.5 kHz, −1 dB at 1.5–2 kHz, −2 dB at
  2.5–3 kHz, −7 dB above 3.5 kHz.

Lined up on whole samples, each 20 ms frame then correlates at about 0.8,
under the line-up's 0.9, and the walk loses track. So the line-up now tries
the sent audio both as it is and half a sample late (each sample averaged
with the one before, which also reproduces the roll-off; correlation 0.997),
and keeps the reading with fewer unmatched frames (`line_up` in
`scenario/sstv.rs`). The check says "half a sample late" when that reading
won. A recording on time keeps the plain reading (it also wins ties).

**Confirmed live after the change** (run `20261120T084004Z`, both scenarios
once, after the two scenarios were folded onto one send flow,
`send_cell`): `sstv_station_to_node` passed every check, and none of its
line-ups chose the half-sample reading. Its figures sit with the 4 s-lead
results: `clean` slowrx 5.77, outside 21.54; `net-1118` slowrx 6.55
(outside, a skip, 22.01); `harsh` slowrx 5.50, 33 rows repaired (outside,
a skip, 59.04). `sstv_station_via_server` in the same run: `clean` 5.66 and
21.86, `net-1118` 6.25 (23.97), and `harsh` **failed again at 15.12**, with
61 timestamps missing from the bridge's output against 26 at its input.
astar-server ran its base config, `/etc/nodebench/node.toml`, identical to
`images/astar-server/node.toml`; no overlay is applied.

### Results (3 runs, 2026-11-20)

Runs `20261120T074855Z`, `075446Z` and `080040Z`, Robot 36. Two trial runs
before them (`073305Z`, `073921Z`, made while the line-up and the second hop's
measurement were being fixed) gave the same decoder figures and are in
brackets. Every picture was sent in full and ended `Sent` after 42.1 s; USRP
lost no frame; astar-server listed the station's call within 0.3 s.

| Profile | slowrx | rows repaired | outside (fitted) | `sstv_station_to_node`, 4 s lead (slowrx; outside) |
|---|---|---|---|---|
| `clean` | 3/3: 5.66, 5.66, 5.87 (5.84, 5.66) | 0 | 3/3: 21.86 ×3 (22.02, 21.86) | 12/12 started, 5.47–5.77, one 39.06; 21.54–44.70 |
| `net-1118` | 3/3: 5.90, 5.73, 5.83 (5.74, 5.92) | 18–21 | skip: 27.66, 48.24, 48.20 | 12/12: 6.17–6.89; skip, 21.95–55.60 |
| `harsh` | **2/3: 9.78, 9.53, 16.79 (9.60, 17.81)** | 46–55 (70) | skip: 63.86, 51.33, 60.32 | **12/12: 5.45–7.14**, 24–37 repaired; skip, 54.2–61.3 |

What each hop carried:

| Profile | Reached astar-server: timestamps missing; frames silent; latest (ms) | astar-server sent the node: timestamps missing; frames silent; latest (ms) | The node's recording |
|---|---|---|---|
| `clean` | 0; 0; 14–57 | 0; 0; 11–27 | whole every run (half a sample late) |
| `net-1118` | 4; 3; 134–145 | 0; 15–30; 12–72 | 20 ms made up at 6.2–6.6 s; 6–12 frames silent |
| `harsh` | 24–25; 23–24; 262–265 | **66–67; 85–107**; 14–16 | 91–235 frames silent; the line-up loses track after the long silences |

On `clean` the bridge and the link carry the Robot 36 picture whole, and the
figures match `sstv_station_to_node`'s slip-free ones. The node did not slip
on `clean` in these runs: the bridge re-times the audio onto its own 20 ms
tick, and frames reached the node at most 27 ms late.

**`--long` (run `20261120T080751Z`, one run).** 90 checks, 2 failed, both
`clean` outside checks, both where the audio was not whole:

| Mode | `clean` slowrx; outside | `net-1118` slowrx (outside, skip) | `harsh` slowrx (outside, skip) | Bridge output, `harsh`: timestamps missing |
|---|---|---|---|---|
| Robot 36 | 5.88; 21.86 | 5.96 (48.93) | 11.18 (59.84) — limit 11.5 | 60 |
| Scottie 2 | 10.57; **37.77, limit 32.1** | 7.94 (17.83) | 7.72 (57.86) | 82 |
| PD120 | 5.18; **49.76, limit 4.7** | 4.52 (4.28) | 12.65 (73.36) — limit 12.9 | 129 |

- **`clean` Scottie 2, outside 37.77: the bridge.** A frame reached
  astar-server 54 ms late (the Mac → VM path; the relay adds nothing on
  `clean`), and the bridge's output has one silent frame at 38.22 s and the
  audio 20 ms later from then on: finding 9's mechanism, at its smallest.
  The node passed that on unchanged.
- **`clean` PD120, outside 49.76: the node.** The bridge's output is whole
  (0 missing, 0 silent, lined up throughout), and frames reached the node at
  most 10 ms late, yet the node made up 40 ms at 51.77 s. That is the
  unexplained `clean` slip already recorded for `sstv_station_to_node`, not
  the bridge. The outside decoder has no repair and PD120's limit (4.7,
  provisional) is tight, so one slip fails it; slowrx read 5.18.
- On `harsh`, Robot 36 and PD120 passed slowrx with little to spare (11.18
  of 11.5, 12.65 of 12.9), with 46 and 110 rows repaired.

### Finding 9: astar-server's bridge turns late audio into silence

**`harsh` passes `sstv_station_to_node` (slowrx 12 of 12, 5.45–7.14) and
fails here in 3 of 6 runs (16.79, 17.81, 15.12; the others 9.5–9.8).** By the
brief's rule the bridge is the suspect, and the capture says it is.

- **What reached astar-server was whole apart from the relay's drops.**
  The first hop's audio lines up against the sent audio with nothing
  dropped or made up. Its only silent frames are the relay's 24–25 dropped
  ones (one at 4.62 s, the 400 ms blackout at 10.04 s, and single frames at
  15.24, 24.66 and 27.46 s). The other frames arrived, up to 265 ms late.
- **What astar-server sent the node was not.** It carried 66–67 missing
  timestamps where the input had 24–25, and long silences where the input
  had audio. The tracked offsets of `harsh` in run `080040Z`
  (`harsh-Robot-36.to-node.wav`), in seconds after key-up, are the same in
  every run, since the relay's schedule is seeded:

  | At (s) | The bridge's output |
  |---|---|
  | 6.52 | 260 ms of silence, then the audio 20 ms later than before |
  | 10.04 | 400 ms of silence (the relay's own blackout) |
  | 11.24 | 20 ms of silence, then +20 ms |
  | 19.62 | 200 ms of silence, then −40 ms |
  | 22.14 | 220 ms of silence, then +40 ms; −60 ms at 22.48 s; 60 ms silent at 22.80 s, then +20 ms |
  | 24.00 | 220 ms of silence, then +40 ms |
  | 31.74 | 180 ms of silence, then −60 ms |
  | 33.66, 34.04, 36.36 | 80, 40 and 80 ms of silence |
  | 36.48 | 240 ms of silence |

  Apart from 10.04 s, none of these is a relay drop. They are where the
  relay's delay bursts hold audio back by 150–265 ms. A minus sign means
  audio was cut out (the output ran ahead), a plus sign means time was
  added.
- **On `net-1118` the same thing is smaller.** The bridge sent 60–80 ms of
  silence at 6.14–6.52 s and 20–60 ms at 19.3, 21.8, 23.7, 31.4 and 36.1 s,
  where the input had only three single-frame relay drops. slowrx repairs
  that (5.7–5.9), so `net-1118` passes.
- **Lateness at the node is low.** The bridge puts out frames on its own
  20 ms tick with fresh timestamps, so the node's jitter buffer sees
  steady audio (14–16 ms latest on `harsh`). The jitter was absorbed at the
  bridge by throwing audio away, where the stock node in
  `sstv_station_to_node` absorbed 265 ms of it with ±20 ms slips.

**The likely cause, not verified by instrumenting the bridge:** the
conference's per-member cushion (`crates/astar-audio/src/dejitter.rs`). By its
own documentation, it holds at most 80 ms (`MAX_HOLD_TICKS`), drops a frame
later than that, ends a talk spurt after 100 ms with nothing queued, and
drops the oldest frames when a burst after a stall overfills the queue.
A 150–265 ms stall followed by a burst does all of those: silence for the
stall, then a fresh anchor and dropped frames. That matches the silences of
180–260 ms followed by offsets of ±20–60 ms.

Not fixed here (fixing the bridge is outside this task). The artifacts for
the failed cells are local, not in git:
`.nodebench/20261120T080040Z/sstv_station_via_server/` (`harsh-Robot-36.wav`,
`.to-server.wav`, `.to-node.wav`, `.frames.txt`) and
`.nodebench/20261120T073921Z/sstv_station_via_server/` (the first failure,
without the hop WAVs).

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
  contexts are gone. The bench's own `[allstar-public]` IAX user (the SSTV
  station's way in) lands in `nodebench-station`, which looks nothing up.
- **No node list download.** `asl3-update-nodelist` is not installed.
- **Not enforced at the network level.** The pod sits on podman's default
  network and *has* a default route. Its silence is configuration plus the
  capture: every UDP packet on `lo` and `eth0` goes to `captures/`.
- **The installs are documented exceptions.** The image build fetches
  packages from repo.allstarlink.org and deb.debian.org. That is package
  installation, not registration.
- `key_refusal` and the transmit time-out are untouched by the bench and
  by every fix above.
- **SSTV sending stays out of shipped builds.** astar-nodebench turns on
  `astar-station`'s `sstv-tx`, so a workspace-wide `cargo build --workspace`
  (or `cargo test --workspace`, or `just build-release`, which ships
  nothing) now compiles the picture sender into every member that uses
  astar-station. Every shipped recipe builds its crate with
  `-p`, where features are resolved for that crate alone, and
  `ci/guard-sstv.sh`'s tree rule checks each shipped crate the same way.
