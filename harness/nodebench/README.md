# nodebench

nodebench tests `astar-server` against a real AllStarLink 3 node. Both run in
podman: a stock ASL3 node (1999) and astar-server (1998) in one pod, plus a
tshark sidecar that records every UDP packet. A host binary, `nodebench`
(crate `crates/astar-nodebench`), drives both sides and checks the results:

- link state on both sides
- audio both ways, as a tone in and an analysis out
- the IAX2 frames on the wire

It is the local stand-in for `iax-nodelab` and the tool for reproducing
`iax-cnlink` (`docs/BACKLOG.md`).

This file covers how to use it. [`RIG.md`](RIG.md) is the rig's reference:
ports, credentials, images, variants and every finding so far.
[`docs/nodebench.md`](../../docs/nodebench.md) is the developer overview.

**No radio exists anywhere in the rig.** The node's only "radio" is a USRP
channel, and the bench is on the other end of it. See [Safety model](#safety-model).

## Quick start

```bash
export CONTAINER_CONNECTION=podman-machine-default   # the Mac (arm64 VM)
harness/nodebench/rig.sh build                       # both images, native arch
just nodebench                                       # up, run all, report, down
```

`just nodebench` passes its flags through to `nodebench run`:

```bash
just nodebench --hold-secs 10                        # a faster pass (the design says 60)
just nodebench node_links_to_server ulaw_only_node   # only these, in this order
just nodebench --keep relink_after_drop              # leave the pod up afterwards
just nodebench --reuse audio_node_to_server          # run against a pod that is already up
cargo run -q -p astar-nodebench -- list              # every scenario id
```

To work by hand on a running rig:

```bash
just nodebench-up                     # rig.sh up; options: --ulaw --debug --register --fill-callerid
harness/nodebench/rig.sh status
harness/nodebench/rig.sh cli 'rpt cmd 1999 ilink 3 1998'
curl -s 127.0.0.1:28730/status
just nodebench-down
```

**Prerequisites.**

- podman 4.9 or newer. On the Mac, set `CONTAINER_CONNECTION` to the arm64
  `podman-machine-default` machine.
- The two images, from `rig.sh build`. `nodebench run` does not build them.
- Host ports 127.0.0.1:24569, 24570, 25038, 28730 and 32001 (UDP and TCP as
  in RIG.md) must be free, and so must 34001/udp, where the bench receives
  the node's audio.

**Exit codes.** `nodebench run` exits 0 when no check failed, 1 when a check
failed, and 2 when the rig would not come up or the arguments were wrong.

## `nodebench run` flags

| Flag | Effect |
|---|---|
| `--topology loopback\|nat` | Which kube file to play. The quick tier is `loopback`. |
| `--keep` | Leave the pod up when the run ends. |
| `--reuse` | Use a pod that is already up: no `kube play` and no teardown. |
| `--replace` | Recreate the pod if one exists. Without it, `run` refuses. |
| `--hold-secs N` | The window for every "link holds" check (default 60). |
| `--json-out DIR` | Base directory for runs (default `target/nodebench`). |
| `--var KEY=VALUE` | Set a `@KEY@` placeholder in the kube file, e.g. `--var CODECS=ulaw` or `--var USRP_HOST=10.0.2.2`. |
| `--rig-dir DIR` | Where `pod.yaml` and `nat.yaml` live (default this directory). |
| `--connection NAME` | podman connection. Defaults to `$CONTAINER_CONNECTION`. |

## Topologies

**loopback** (`pod.yaml`, the quick tier). One pod with one network
namespace. The node is `127.0.0.1:4569` and astar-server `127.0.0.1:4570`,
and they reach each other over the pod's loopback. Each service's host port
is its own port plus 20000, on the host's 127.0.0.1 only. The offset is there
because other tests on the same Mac dial 127.0.0.1:4569 and 8730. A
`usrp-relay` sidecar carries the node's USRP audio to and from the host, and
a `capture` sidecar runs tshark on `lo` and `eth0` into a ring of pcapng
files.

**nat** (`nat.yaml`, the soak tier only). The node sits behind a router pod
doing MASQUERADE with a settable conntrack UDP timeout, the way a ClearNode
sits behind a home router. Both networks are podman `--internal` networks,
which have no default route and so no way out of the podman VM. That is the
one exception to the "everything on 127.0.0.1" rule; Rob approved it on
2026-10-11. Host ports are offset by 30000, so it can run beside the loopback
pod. `nodebench soak --topology nat --udp-timeout S` plays it. See the header
of `nat.yaml`.

## Scenarios (the quick tier)

Each scenario is a file in `crates/astar-nodebench/src/scenario/`. Each one
returns named checks, and each check passes, fails, or is skipped with a
reason.

The status column is from the full end-to-end run on 2026-10-12 (run
`20261013T025532Z`, arm64, default 60 s holds, `work/iax-fixes`): 15
scenarios, 100 of 100 checks pass, after the fixes for every RIG.md finding.
A **FAIL** in a later run is a real astar-server bug until shown otherwise.

| id | What it checks | Status |
|---|---|---|
| `rig_ready` | app_rpt loaded 1999, AMI logs in, astar-server is listening | PASS |
| `node_links_to_server` | `ilink 3 1998`: both sides report the link and it holds; app_rpt's `!NEWKEY1!` is answered | PASS (finding 6 fixed) |
| `server_links_to_node` | `POST /link connect 1999` at 127.0.0.1:4569: both up, holds; app_rpt's `!NEWKEY1!` is answered | PASS (findings 1 and 6 fixed) |
| `monitor_link` | `ilink 2`: up, audio one way only | PASS (findings 6 and 7 fixed) |
| `disconnect_from_each_side` | `ilink 1` (5 s) and `/link disconnect` (2 s): both sides agree it is gone, and who hung up; astar-server says `!!DISCONNECT!!` first, on the call clock | PASS (finding 3 fixed; the server side clears in about 0.2 s) |
| `ulaw_only_node` | ClearNode stand-in: ulaw only, links, holds, ulaw both sides, audio passes | PASS |
| `relink_after_drop` | `ilink 1` then `ilink 3` at once: the new link holds, and the old HANGUP is sent once and ACKed with `oseqno + 1` | PASS (finding 4 fixed; it was the `iax-cnlink` drop) |
| `codec_matrix` | node slin16/slin/ulaw × server `codec_policy`, both dial directions: the negotiated codec agrees | PASS, 24 of 24 (finding 5 fixed) |
| `auth_variants` | astar-server `auth = off/optional/required` against anonymous, right and wrong secrets; a "refused" cell needs the node's NEW and a REJECT/HANGUP on the wire | PASS, 6 of 6 |
| `audio_node_to_server` | USRP 1 kHz into the node, heard by an IAX probe on astar-server's bridge: frequency, level, SNR (median per 20 ms frame, dropouts counted), onset, loss | PASS |
| `audio_server_to_node` | the probe sends 700 Hz into the bridge, recorded from the node's USRP | PASS (finding 6 fixed) |
| `keepalive_exchange` | PING/PONG, LAGRQ/LAGRP and app_rpt TEXT keepalives are each answered (pcap) | PASS |
| `dtmf_link_command` | `[dtmf]` on: the probe sends `*11999#` and the server drops 1999 | PASS (the doubled `#` went with finding 4; the drop takes 0.1 s since finding 3) |
| `calltoken_retransmit` | raw IAX2 from the host: a NEW retransmitted without its token gets CALLTOKEN again, not REJECT, and the token NEW is accepted | PASS (fixed `ca622a2`) |
| `register_with_node` | astar-server `[register]` against the node: registered, refreshes, call numbers bounded | PASS (findings 2, 8 fixed: one call number); the node is restarted afterwards |

The bench only ever sends `*1`, never `*2` or `*3`, because those can dial a
node with no address, and that would query AllStar DNS.

## Reading a report

Each run gets a directory, `target/nodebench/<run-id>/`, where the run id is
a UTC timestamp such as `20261011T212733Z`:

| Path | What |
|---|---|
| `report.json` | The whole run: environment (podman version, image ids, git sha), then each scenario's checks with `status`, `detail`, `measured` and `unit`, its notes, and its artifacts |
| `<scenario>/last-frames.txt` | **Only when the scenario failed.** The last 50 IAX2 full frames before the drop, or before the end, plus 1 s after. Each frame is decoded with its IEs: FORMAT, CALLTOKEN, CAUSE and so on. |
| `<scenario>/asterisk-full.tail.log`, `astar-server.tail.log` | Only on failure. The last 800 lines of the node's full log and 400 of astar-server's. |
| `<scenario>/<cell>.frames.txt` | Matrix scenarios: the frames of each failed cell |
| `logs/` | Every container's log and the node's Asterisk logs, collected at the end |
| `pcap/` | The tshark ring buffer, for Wireshark |

Stdout has the same thing as a table: one line per scenario, its checks
indented underneath with the measured value, then `note:` lines.

Read a failure in this order:

1. The check's `detail`. It is written to be enough on its own. For
   example: "broke at 2.3 s (last good 1.2 s): server: call 6 is gone".
2. The notes. Several scenarios say what the frames show, such as "astar's
   ACK carried iseqno 11" or "NUL byte(s) at [8]".
3. `last-frames.txt`, then the log tails, then the pcap.

## Adding a scenario

1. Create `crates/astar-nodebench/src/scenario/<name>.rs` and implement
   `Scenario`. `smoke.rs` is the smallest example.

   ```rust
   pub struct MyThing;
   impl Scenario for MyThing {
       fn id(&self) -> &'static str { "my_thing" }          // stable: CLI and reports use it
       fn describe(&self) -> &'static str { "one line for `list` and the report" }
       fn run(&self, ctx: &mut Ctx) -> Outcome {
           let mut o = Outcome::new();
           // ...drive ctx.node / ctx.server, push Check::pass / fail / skip / expect
           o
       }
   }
   ```

2. Add `mod <name>;` and a `Box::new(...)` line to `registry()` in
   `scenario/mod.rs`. Order matters: `register_with_node` stays last,
   because it can exhaust the node's call numbers.
3. Build on `scenario/common.rs` rather than raw podman:
   - `clean_start(ctx)` first. It drops every link and waits until the
     node's IAX channel list is empty, so one scenario cannot leak into the
     next.
   - `node_dials_server` and `server_dials_node` bring a link up, and
     `hold_link` holds it with `ctx.tuning.hold`.
   - `with_node_overlay` and `with_server_overlay` change the node's
     `iax.conf`/`rpt.conf` or astar-server's `node.toml`, restart only that
     container, and always restore it afterwards.
   - `usrp_probe`, `iax_probe`, `analyse_tone` and `tone_checks` cover audio.
   - `frames_since_start` and `explain_teardown` read the pcap.
4. When something drops, call `ctx.mark_drop()`. The failure artifacts are
   then cut at the drop.
5. Every check needs a `detail` that says what was seen, and `.measured(v,
   unit)` when there is a number.
6. Do not send anything that could reach beyond the pod (see below). Run
   `cargo test -p astar-nodebench` and `cargo clippy -p astar-nodebench
   --all-targets -- -D warnings`.

## Soak tier and the results page

- `nodebench soak --hours N [--topology loopback|nat] [--udp-timeout S]`
  holds one link for hours. It logs every up and down edge with who hung up,
  and writes `soak.jsonl`, `summary.json` and `drops/` into its run
  directory.
- A sample that says "down" is taken again 500 ms later before it counts.
  A sample on which the node (AMI) or astar-server (HTTP) cannot be asked
  at all is skipped with a `note` event, not counted as a drop; only six
  blind samples in a row end the link.
- `nodebench serve` serves the run directories on http://127.0.0.1:8740. It
  refuses any address that is not loopback.

`nodebench soak --help` and `nodebench serve --help` have the details.

## CI

`ci.yml` has a `nodebench` job. It is guarded on
`github.repository == 'rcludwick/astar-private'` like every other job, and it
is **off unless dispatched with `run_nodebench: true`**. The
`nodebench_scenarios` and `nodebench_hold_secs` inputs narrow it.

The job's steps:

1. A preflight that fails with a pointer here when podman cannot nest.
2. `rig.sh build`.
3. `nodebench run --replace --var USRP_HOST=10.0.2.2`.
4. Upload `target/nodebench/` as `nodebench-report`.
5. `rig.sh down`.

**It cannot run on the IONOS runner today.** As of 2026-10-11, the
`localhost/gh-runner` image has no podman. The runner container is also
created with podman's default confinement (`gh_runner` role, no devices and
no `security_opt`), and under that a nested rootless podman cannot even map
its user namespace. Mounting the box's podman socket instead does not work
either:

- The pod's ports would then be published on the box, not on the runner
  container's 127.0.0.1, where the bench looks for them.
- Any CI job could `podman inspect` the other runner containers and read the
  PAT in their environment.

### What the runner needs

This recipe was proven on the Mac on 2026-10-11, with
[`ci/nested-trial.sh`](ci/nested-trial.sh), in an Ubuntu 24.04 stand-in of
the runner image ([`ci/runner-standin.Containerfile`](ci/runner-standin.Containerfile)).
That image runs podman 4.9.3 with CNI and slirp4netns, as Ubuntu's own
package does, inside rootless podman 5.7. In the stand-in:

- the real rig came up in 1 s;
- `ilink 3` linked;
- tshark captured;
- six scenarios gave the same results as on the Mac. That includes USRP
  audio both ways, which proves the 10.0.2.2 path.

It is **not yet verified on amd64**, nor on the box's Debian 13 host.

**1. The image** (`gh-runners/runner/Containerfile`): add these, in the root
section:

```dockerfile
RUN apt-get update && apt-get install -y --no-install-recommends \
        podman uidmap fuse-overlayfs slirp4netns passt catatonit libcap2-bin \
    && rm -rf /var/lib/apt/lists/*
# File capabilities, not setuid. With only the setuid bit, newuidmap fails
# inside a rootless container: "write to uid_map failed: Operation not permitted".
RUN chmod 0755 /usr/bin/newuidmap /usr/bin/newgidmap \
    && setcap cap_setuid=ep /usr/bin/newuidmap \
    && setcap cap_setgid=ep /usr/bin/newgidmap
# The inner user namespace: every outer id except runner's own 1001.
RUN printf 'runner:1:1000\nrunner:1002:64534\n' > /etc/subuid \
    && printf 'runner:1:1000\nrunner:1002:64534\n' > /etc/subgid
```

Then, as `runner`, add `~/.config/containers/storage.conf` and
`containers.conf`, both shown in `ci/runner-standin.Containerfile`. They set
up:

- the overlay driver with `mount_program = "/usr/bin/fuse-overlayfs"`,
  because the container's root is already overlay;
- `cgroup_manager = "cgroupfs"`;
- `events_logger = "file"`;
- `network_cmd_options = ["allow_host_loopback=true"]`. With it, a pod
  reaches the runner's 127.0.0.1 at `10.0.2.2`, which is how the node's
  USRP transmit audio gets back to the bench.

**2. The container** (the `gh_runner` role's `podman_container`): add these
options. Each was found by removing it and watching the nested pod fail:

| Option | Without it |
|---|---|
| `--device /dev/fuse` | `fuse-overlayfs: cannot mount`, so there is no storage |
| `--device /dev/net/tun` | slirp4netns cannot make the pod's network |
| `--security-opt label=disable` | On the Mac's Fedora VM, SELinux denies slirp4netns `/dev/net/tun` (`Permission denied`). Debian runs AppArmor, not SELinux, so it is probably a no-op on the box. Keep it anyway: podman's own nested recipe uses it. |
| `--security-opt unmask=ALL` | `/proc/sys` is read-only, so CNI cannot enable forwarding, and the masked `/proc` paths stop crun mounting a fresh `proc` |
| `--security-opt seccomp=unconfined` | the default profile gates `sethostname`, `mount` and the like on CAP_SYS_ADMIN, so crun fails with `sethostname: Operation not permitted` |
| `--security-opt apparmor=unconfined` | **Untested; probably needed on the box.** The Mac VM has no AppArmor. On Debian, podman applies its `containers-default` AppArmor profile, which is modelled on Docker's and denies `mount`, and the nested podman has to mount. |

Also recommended, but not tested: a named volume for
`/home/runner/.local/share/containers`. It keeps the nested image store,
and its astar-server build cache, warm across container recreation.

**The cost.** Those options loosen the runner container's confinement:
seccomp is off, `/proc` is unmasked, and SELinux labels (and probably
AppArmor) are off. It stays a rootless container inside the `github`
user's user namespace, with no host root. But a CI job gets a wider kernel
surface, on a box that also runs other repositories' runners. Two ways to keep that contained:

- Apply the options only to the `astar-private` runner. A `runner_repos`
  entry would need a per-repo `nested: true` flag in the role.
- Or add a second astar-private runner just for this, registered with an
  extra label (the entrypoint hard-codes `--labels ionos` today), and give
  this job that label.

That choice is Rob's. Nothing in `gh-runners` has been changed.

**3. The box.** The `github` user's subuid/subgid range must be at least
65536 wide, because the inner map uses outer ids 1 to 65535. `/dev/fuse` and
`/dev/net/tun` must exist; they normally do on Debian. The quick tier is
light: two small containers and two sidecars. But the job also builds
astar-server in release mode inside the nested store, so `CARGO_BUILD_JOBS`
is set to 2 for the 3.8 GB box.

**Still unknown until it runs on the box:**

- whether AppArmor on the box needs `apparmor=unconfined` as well;
- whether ASL3's amd64 packages behave as the arm64 ones do;
- whether the `--internal` networks of the nat topology work nested. The
  quick tier does not use them;
- how long the full quick tier takes with 60 s holds. The job's
  `timeout-minutes: 90` is a ceiling, not a measurement.

## Safety model

These are the rules the bench is built on. CLAUDE.md "Safety rules" records
the nodebench exception.

- **No radio.** No sound card, serial port or USB device is passed into any
  container. The node's rxchannel is USRP to the bench, and `chan_simpleusb`
  and `chan_usbradio` are not loaded. The bench may call astar-server's
  `/key`, because the only things that can hear it are the containerised
  node and the probes.
- **127.0.0.1.** In the loopback topology every node-to-node packet is on
  the pod's loopback. Every published port is on the host's 127.0.0.1, and
  the relay's only outbound target is the host itself.
- **The nat exception.** The nat topology uses two podman `--internal`
  networks, which have no route out (Rob approved this on 2026-10-11).
- **No AllStar network.** AllStar DNS is never queried
  (`node_lookup_method = file`, both nodes in `[nodes]`). There is no portal
  registration, no HTTP registration, no statpost, no `func_curl`, and no
  node list download. Node numbers 1999 and 1998 are private test numbers.
  DTMF scenarios never send `*2` or `*3`.
- **Test-only credentials.** The secret is `nodebench-loopback-only`, and it
  is committed on purpose: nothing outside the podman VM can reach it.
- **The product's guards stay.** `key_refusal` and the transmit time-out are
  untouched. The bench changes no astar-server code.
- **Not enforced at the network level.** The loopback pod sits on podman's
  default network, which has a default route. Its silence comes from
  configuration, and the capture sidecar records every UDP packet, so a
  stray one would show. The image build fetches packages from
  deb.debian.org and repo.allstarlink.org; that is package installation,
  not registration.
- **Hardware-gated suites never run here.** `IAX_THUMBDV_TESTS` stays unset.
  The design's later `--with-vocoder` release tier is Rob's to run.
