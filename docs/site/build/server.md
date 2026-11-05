---
icon: lucide/server-cog
---

# Building astar-server

The node daemon is a plain cargo binary. Of the three deliverables it is the
simplest to build and the only one meant to run unattended.

!!! info "This page is the build only"

    What the daemon does, every configuration knob, and the HTTP + SSE control
    API are covered separately under [astar-server](../server/index.md) — which
    also lists what is still unfinished, and where to start if you want to work
    on it.

## Build it

```bash
cargo build --release -p astar-server
```

The binary lands at `target/release/astar-server`. On Linux it needs the two
dev packages from [Prerequisites](prerequisites.md#linux); on macOS, nothing
beyond Rust. No Xcode, no Swift, no xcframeworks.

## Run it

```bash
cp deploy/node.toml.example node.toml    # then edit it
just node                                # astar-server serve --config node.toml
```

Or directly, without `just`:

```bash
astar-server serve --config node.toml    # HTTP + SSE control channel
astar-server tui   --config node.toml    # interactive stdin menu
```

`just node` and `just node-tui` source a local `.env` first, so under
`[secrets] source = "env"` the `ALLSTAR_NODE` / `ALLSTAR_SECRET` variables
reach the process without ever being written into the config file. A node
on the default `source = "file"` reads `credentials.toml` beside its
`node.toml` instead and needs no `.env`. Under `just node` that is
`credentials.toml` in the repository root, where the settings page writes
real passwords and tokens; `.gitignore` covers it, so keep it that way.

If the config path does not exist, `serve` writes a commented template there
and carries on with safe defaults rather than exiting — a daemon under
`--restart=always` must not crash-loop over a missing file.

!!! danger "Credentials stay out of `node.toml` unless it is private"

    By default the config file holds no credential: they come from
    `credentials.toml`, the environment, or a POST to the loopback control
    port. Only `[secrets] source = "config"` puts them in `node.toml`, and then
    the file must be mode `0600`. Wherever they come from, secrets are
    connect/init arguments only: never stored on a station, never in
    snapshots, events or errors, never logged.

    Bind the control channel to `127.0.0.1`. It can key a transmitter.

## One file: the static release

For a server, astar-server builds as **one static executable**: no shared
library, no sound-device stack, no package to install beside it. LAME,
Codec 2 and the software D-Star vocoder (all LGPL, see `LICENSE-EXCEPTIONS.md`),
the Opus codec, the TLS stack and the web pages are all inside.

```bash
just server-release                              # x86_64-unknown-linux-musl
just server-release aarch64-unknown-linux-musl   # ARM servers
```

It builds on the host with `cargo-zigbuild`, so it needs `zig`,
`cargo-zigbuild` and `cmake` (for the Discord feature's Opus library), and
writes `target/server-release/<target>/astar-server`. The recipe ends by
running `ci/check-server-release.sh`, which checks the binary is static and
holds what the release promises. `astar-server --features` prints what a
binary was built with.

The feature list is the `release` feature in `crates/astar-server/Cargo.toml`:
`stream`, `stream-mp3`, `codec2-static`, `public-tls` and `discord`. It
leaves out `device-audio` on purpose: a server runs `[audio] backend =
"none"`, and a static musl binary cannot link the ALSA library. A release
binary told to use a sound device says so once and uses `none`.

### Deploying it: one executable, one config

A node is the binary plus one `node.toml`:

- **State** (the ACME account and certificates, later the settings store) is
  created at run time in `state/` beside the config, or in systemd's
  `$STATE_DIRECTORY`. Nothing is deployed into it.
- **Credentials** can sit in `node.toml` itself under `[secrets] source =
  "config"`: `secret` (the node password), `portal`, `discord`, `porkbun_pk`
  and `porkbun_sk`. Keep the file mode `0600`; the server warns at start-up
  when group or others can read it. The environment works too
  (`source = "env"`).
- A missing `node.toml` is written as a commented template on first start.

The binary prints a hardened systemd unit for itself:

```bash
astar-server --print-systemd-unit | sudo tee /etc/systemd/system/astar-server.service
sudo install -m 0755 astar-server /usr/local/bin/
sudo install -d -o allstar -g allstar /etc/astar
sudo systemctl enable --now astar-server
```

The unit runs as the `allstar` user with `StateDirectory=astar` and grants
exactly one privilege, `CAP_NET_BIND_SERVICE`, so the
[public page](../server/listening.md) can bind ports 80 and 443 without
root and without a port sysctl. With it, no container is needed at all.

## A container image

`deploy/` holds the code-side artifacts for running the daemon on a server:
Containerfiles, a build script, and a manual deploy loop.

```bash
deploy/build.sh        # just server-release x86_64-unknown-linux-musl
podman build --platform linux/amd64 \
  -t astar-server:smoke -f deploy/Containerfile.app deploy/out
```

`deploy/Containerfile.base` is the rarely-changing base layer;
`Containerfile.app` layers the binary on top. `deploy/smoke/` has a
config for a local smoke run that needs no server at all.

`deploy/deploy-vps.sh` and `deploy/ship-base.sh` compile, ship and restart
against a host of your choosing — set `ASTAR_VPS=user@host` first. They are
built around one particular deployment and are best read as a worked example
rather than a general-purpose tool.

## Operational notes

`astar-server` listens on **UDP 4569** for inbound IAX2, so that port has to
reach it — a port-forward, a firewall rule, or both. The HTTP control channel
is separate and should stay on loopback.

Registration, answer policy, the call cap, the caller allowlist, announcements
and DTMF command execution are all configuration rather than build options.
DTMF command execution is **off by default**, because enabling it lets any
connected member command your links.

## Next steps

* [astar-server](../server/index.md) — what it does, and its current state.
* [Configuration](../server/configuration.md) — every section of `node.toml`.
* [Control API](../server/control-api.md) — the HTTP and SSE surface.
* [The engine](engine.md) — the workspace the daemon is a crate in.
* [Verifying a build](verifying.md) — the gates.
