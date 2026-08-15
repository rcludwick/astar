# deploy/ — astar-server build artifacts (iax-4703)

Cloud AllStar hub: `astar-server serve`, bridge mode, running under a plain
rootful podman container on a Debian host.

Set `ASTAR_VPS=user@host` to point the ship/deploy scripts at your own server.
Box provisioning, the runner fleet, and the automated deploy pipeline live in
**rcludwick/gh-runners** (iax-eeff migration) — this directory keeps only the
code-side artifacts: Containerfiles, the local build, and the manual deploy
loop. Spec: `docs/superpowers/specs/2026-07-04-iax-4703-vps-node-podman-deploy-design.md`.

**Automated path:** every green master push dispatches gh-runners'
`deploy-node` workflow (build on the runner fleet → ship → restart →
health gate). The scripts below are the manual/Mac fallback.

## Daily loop (code change → live)

    deploy/deploy-vps.sh      # compile (warm ~1-2 min) + ship binary + restart

## One-time / rare

    # box provisioning: ansible-playbook site.yml in rcludwick/gh-runners
    deploy/ship-base.sh       # first deploy + whenever Containerfile.base changes

## Operator-only steps (cannot be scripted)

- Paste the node password into `[secrets] secret = "…"` in
  `/etc/iaxnode/node.toml` on the VPS, then `sudo podman restart iaxnode`.
  (The env file is retired.)
- Open UDP 4569 inbound in the IONOS Cloud Panel firewall.

## Ops crib sheet (on the VPS)

    sudo podman ps                                # container state
    sudo podman logs -f iaxnode                   # follow logs
    sudo podman restart iaxnode                   # restart (config re-read)
    sudo iaxnode-run                              # recreate container (new image)
    curl -s http://127.0.0.1:8730/status          # control channel (loopback only)
    sudo -e /etc/iaxnode/node.toml && sudo podman restart iaxnode

## Local smoke test (no VPS needed)

    deploy/build.sh
    podman build --platform linux/amd64 -t astar-server:smoke -f deploy/Containerfile.app deploy/out
    # then run with deploy/smoke/node.toml mounted — see Task 3 of the plan.
