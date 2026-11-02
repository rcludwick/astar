# deploy/ — astar-server build artifacts (iax-4703)

Cloud AllStar hub: `astar-server serve`, bridge mode, running in a rootless
podman container, owned by the `allstar` service user, on a Debian host.

Set `ASTAR_VPS=user@host` to point the ship/deploy scripts at your own server.
Box provisioning, the runner fleet, and the automated deploy pipeline live in
**rcludwick/gh-runners** (iax-eeff migration) — this directory keeps only the
code-side artifacts: Containerfiles, the local build, and the manual deploy
loop.

**No automated path today.** gh-runners' `deploy-node` workflow (build on the
runner fleet → ship → restart → health gate) runs only on a
`repository_dispatch`, and nothing in this repo sends one: `ci.yml` is
`workflow_dispatch` only and has no notify step. Its old sender was the
standalone iaxclient-rs repo's `notify-deploy` job, retired when iaxclient-rs
was merged in on 2026-08-11 (last dispatched run: 2026-08-05), and the workflow
still checks out `rcludwick/iaxclient-rs`. Deploys are the scripts below.

## Daily loop (code change → live)

    deploy/deploy-vps.sh      # compile (warm ~1-2 min) + ship binary + restart
                              # + deploy/verify.sh, then prune old images

## Verify

`deploy/verify.sh` (`just deploy-verify`) checks a running node from the Mac
or a runner. `deploy-vps.sh` runs it after every restart.

    ASTAR_VPS=allstar@<vps> deploy/verify.sh                 # box checks
    ASTAR_VPS=allstar@<vps> ASTAR_PUBLIC_HOST=<name> deploy/verify.sh   # + public
    deploy/verify.sh --json                                  # for machines

**It only observes.** Over ssh it runs read-only commands (`podman inspect`,
`podman images`, `podman logs`, `df`, `ss`) and a GET of the loopback
`/status`. Publicly it sends GETs, plus one POST to `/` that must come back
405. It never keys, links, restarts, prunes or writes anything, so it is safe
to run against the live node at any time. `ASTAR_VPS` can be `root@` or
`allstar@`; a root login drops to `allstar` for podman.

It prints one line per check (PASS, FAIL or SKIP) and exits non-zero on any
FAIL.

| Check | Fails when |
|---|---|
| `box-ssh` | the box can't be reached (the rest are then SKIPped) |
| `container-running` | `iaxnode` is missing or stopped |
| `container-image` | the container wasn't started from `localhost/iaxclient-node:latest`, or runs an image other than the one that tag points at now. This is the 2026-09-02 failure, where a deploy built an image nothing ran. |
| `container-restarted` | the container started before the deploy began (`ASTAR_DEPLOYED_AFTER`, box clock; SKIP when unset) |
| `image-count` | more than 6 images (`ASTAR_MAX_IMAGES`). This is the 2026-09-27 disk-full incident. |
| `disk` | `/` is 80% full or more (`ASTAR_DISK_MAX`) |
| `status-registered` | `/status` doesn't say `registered: true` within 60 s (`ASTAR_REGISTER_WAIT`) |
| `status-listening` | `/status` doesn't say `listening: true` |
| `log-secrets` | the last 500 log lines contain `apikey`, `porkbun`, `token`, a `secret =` assignment, or the literal value of any secret, password, apikey or token in `node.toml`, `credentials.toml` or `portal.env`. The match runs on the box. Only line numbers come back; the content and the values never leave the box. |
| `udp-4569` | nothing is bound to UDP 4569 |
| `dns` | the host's A records aren't exactly the box's IP (`ASTAR_VPS_IP` when `ASTAR_VPS` isn't an IP) |
| `tls-chain` | the certificate doesn't chain to a trusted root or doesn't cover the host |
| `tls-expiry` | the certificate has 14 days or fewer left (`ASTAR_CERT_MIN_DAYS`) |
| `http-root` | `GET /` isn't 200 |
| `public-status` | `GET /public/status` isn't 200 JSON, or carries an address-like key (`addr`, `ip`, `port`, `host`, ...) or an IPv4 or host:port value anywhere |
| `hidden:/key` and the rest | `/key`, `/link`, `/secrets`, `/settings` or `/status` is anything but 404 publicly (checked with GET only) |
| `post-405` | `POST /` isn't 405 |
| `wrong-host` | a foreign `Host` header gets anything but a 4xx or a refused connection |
| `stream-mp3` | `/stream.mp3` is neither 200 `audio/mpeg` with bytes flowing nor 503 with a reason. At most 64 KB is read. |
| `http-80` | port 80 serves something other than a redirect to `https://<host>` (not listening is fine) |

The public checks SKIP until `ASTAR_PUBLIC_HOST` is set, which means once the
listener (public bind on 443, ACME) is deployed.

When verify fails inside `deploy-vps.sh`, the script prints the rollback
command and stops before pruning, so the previous image is still on the box:

    ssh allstar@<vps> 'podman tag <previous image id> localhost/iaxclient-node:latest && /home/allstar/bin/iaxnode-run'

The tests (`just deploy-verify-test`, part of `just ci`) drive verify.sh
against `deploy/test/fake-ssh`, which runs the remote commands locally
against canned podman, df, ss and curl, and against `deploy/test/fake_server.py`,
a fake public bind on 127.0.0.1 behind a throwaway CA. Every check has a case
that makes it fail. They use no network.

gh-runners' `deploy-node` workflow has its own inline `/status` gate in
`ci/deploy-node-vps.sh`; it should call this script instead (a follow-up in
that repo).

## One-time / rare

    # box provisioning: ansible-playbook site.yml in rcludwick/gh-runners
    deploy/ship-base.sh       # first deploy + whenever Containerfile.base changes

## Operator-only steps (cannot be scripted)

The node runs rootless as `allstar` (host uid 1000) with `--userns=keep-id`.
Its config directory is `/home/allstar/iaxnode` on the host, mode 0700 and
owned by `allstar`, mounted at `/etc/iaxnode` in the container, where the
daemon runs as uid 1000 too. It can already write there, so the settings
page's `file` badge needs no ownership fix and nothing here needs sudo.

- **`node.toml` on the VPS is ansible-managed.** gh-runners' `iaxnode` role
  renders it from `roles/iaxnode/templates/node.toml.j2` —
  `[secrets] source = "config"` with the vault secret inline — and restarts
  the node whenever the rendered file changes. So the next `site.yml` run
  silently overwrites whatever the settings page saved, the credentials move
  below included, and restarts the node on the template's config. Page
  edits, and that move, only stick once the template itself carries
  `source = "file"` with no inline secret, or the role stops templating
  `node.toml`. Until then, make any change you want to keep in the template.
- Set the node password from the settings page. Tunnel the control port
  (`ssh -N -L 8730:127.0.0.1:8730 allstar@<vps>`), open
  `http://127.0.0.1:8730/settings`, and use **Set** beside the node password.
  On a node still on `[secrets] source = "config"` the page offers **Move
  credentials to credentials.toml**, which writes the secret to
  `credentials.toml` beside `node.toml` (`/home/allstar/iaxnode/` on the
  host, mode 0600), flips the source to `file` and blanks the inline
  `secret`, in that order. Then **Restart node** from the same page. (The
  env file is retired; so is pasting into `node.toml`.) Read the previous
  point first: on the VPS today, ansible undoes this on its next run.
- Open UDP 4569 inbound in the IONOS Cloud Panel firewall.

## Ops crib sheet (on the VPS, as `allstar`; no sudo)

    ssh allstar@<vps>
    podman ps                                     # container state
    podman logs -f iaxnode                        # follow logs
    podman restart iaxnode                        # restart (config re-read)
    ~/bin/iaxnode-run                             # recreate container (new image)
    curl -s http://127.0.0.1:8730/status          # control channel (loopback only)
    $EDITOR ~/iaxnode/node.toml && podman restart iaxnode   # lasts until ansible re-renders it; a settings page that was open answers 409 on its next save until it is reloaded

## Local smoke test (no VPS needed)

    deploy/build.sh
    podman build --platform linux/amd64 -t astar-server:smoke -f deploy/Containerfile.app deploy/out
    # then run with deploy/smoke/node.toml mounted at /etc/iaxnode/node.toml (the ENTRYPOINT's --config path).
