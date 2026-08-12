---
icon: lucide/server-cog
---

# Building astar-server

The node daemon is a plain cargo binary. Of the three deliverables it is the
simplest to build and the only one meant to run unattended — see
[astar-server](../server/index.md) for what it actually does, and
[Configuration](../server/configuration.md) for every knob.

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

`just node` and `just node-tui` source a local `.env` first, so
`ALLSTAR_NODE` / `ALLSTAR_SECRET` reach the process without ever being written
into the config file.

If the config path does not exist, `serve` writes a commented template there
and carries on with safe defaults rather than exiting — a daemon under
`--restart=always` must not crash-loop over a missing file.

!!! danger "Never put a secret in `node.toml`"

    The config file is **secret-free by design**. The registration secret comes
    from the environment or is POSTed to the loopback control port at run time.
    Secrets are connect/init arguments only: never stored on a station, never
    in snapshots, events or errors, never logged.

    Bind the control channel to `127.0.0.1`. It can key a transmitter.

## A container image

`deploy/` holds the code-side artifacts for running the daemon on a server:
Containerfiles, a build script, and a manual deploy loop.

```bash
deploy/build.sh
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

* [astar-server](../server/index.md) — what the daemon does.
* [Configuration](../server/configuration.md) · [Control API](../server/control-api.md)
