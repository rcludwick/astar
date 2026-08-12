# ci/ — pipeline helpers and runner prerequisites

The pipeline is [`../.gitlab-ci.yml`](../.gitlab-ci.yml). This directory holds
the scripts it calls, plus the operational facts you need before it can run.

| script | purpose |
| --- | --- |
| `guard-safety-env.sh` | Hard-fails if any hardware / live-network opt-in is armed. Runs before every compiling job. |
| `guard-no-git-deps.sh` | The repo has no git dependencies; `ambe-thumbdv` is vendored with its licences and must stay. |
| `guard-distribution-claims.sh` | astar has never been released — fails on any invented install channel. |
| `guard-spdx-headers.sh` | Every first-party `.rs` / `.swift` / `.sh` / `.py` file carries the `AGPL-3.0-only` SPDX header the README and the docs site promise. `vendor/` and the vendored C libiax2 are exempt — different licences. |
| `build-docs.sh` | Builds the Zensical site (`docs/site` → `docs/.site`) with `--strict`. Shared by GitLab and the dormant Pages workflow. |

Each script is runnable by hand from the repo root and prints a one-line
success message, so you can reproduce a red pipeline locally in seconds.

## Read this first: the pipeline is not live yet

`.gitlab-ci.yml` targets a GitLab runner on the IONOS box. **No such runner is
registered and no GitLab project for astar exists.** What is deployed on
`192.0.2.20` today is a fleet of self-hosted *GitHub Actions* runners,
managed as code in `rcludwick/gh-runners` (Ansible, rootless podman, one
container per runner). There is no `gitlab-runner` binary on the box, no
gitlab.com remote on any repo, and no `glab` CLI on the workstation.

The tags in the pipeline mirror the real Actions labels so both name the same
machines, but they are **registration requirements, not discovered facts**.

### To make the Linux half real

1. Create the GitLab project and push this repo to it.
2. On `192.0.2.20`, install `gitlab-runner` and register it with the
   **`shell` executor** — not `docker`. The box runs rootless podman with no
   Docker daemon, and 3.8 GB of RAM makes per-job image pulls painful:

   ```console
   $ gitlab-runner register \
       --url https://gitlab.com/ \
       --executor shell \
       --tag-list "ionos,linux,x64" \
       --description "ionos-astar-1"
   ```

3. Give the runner's service user the toolchain below. Jobs install nothing.

### What the runner image / host must carry

The GitHub Actions fleet bakes its dependencies into `runner/Containerfile` and
its workflows install nothing; keep that discipline here. Adding a dependency
is an infrastructure change, not a pipeline change.

* `build-essential`, `pkg-config`
* `libasound2-dev` (cpal/ALSA), `libudev-dev` (serialport)
* `python3`, `python3-venv`, `python3-pip`
* rustup **stable** with `rustfmt` + `clippy` (workspace MSRV is 1.89)
* `git`, `ripgrep`
* For `gui-linux`: `xvfb`, `libxcursor1`, `libxrandr2`, `libxi6`,
  `libxkbcommon-x11-0`, `libxcb-shape0`, `libxcb-xfixes0`, `libxcb-shm0`,
  `fontconfig`, `fonts-dejavu-core`

`just` is **not** installed and the pipeline does not use it — recipes are
spelled out as cargo invocations. Keep the two in step by hand.

Network access is needed for `cargo install cbindgen` (once, then cached in the
persistent `CARGO_HOME`) and for the one `pip install` in the docs build.
Everything else is `--locked --offline`-clean because the tree has no git
dependencies.

## The macOS hole, stated plainly

The SwiftUI app, the Swift bindings, `swift format`, and the cargo-xwin Windows
cross-build all need macOS with a full Xcode. The IONOS box is Debian on
x86-64: wrong OS, wrong arch, no Swift toolchain.

Rob's Mac hosts self-hosted *GitHub Actions* runners (`mac-astar-1`,
`mac-iaxclient-rs-1`, Xcode 26.6, Apple Silicon). It hosts no gitlab-runner, so
**nothing answers the `macos, arm64` tags.**

The `macos` stage jobs are therefore `when: manual` + `allow_failure: true`.
They are a visible hole in the pipeline, not coverage. Triggering one before a
runner exists leaves it pending, which is the honest result.

**Until a Mac runner is registered, the Swift side is gated by hand:** run
`just ci-full` on the Mac before merging anything under `apps/macos/`,
`bindings/swift/`, or `bindings/swift-serial/`.

To close the hole: `gitlab-runner register --executor shell --tag-list
"macos,arm64"` on the Mac, as a launchd service under Rob's user so it inherits
the Xcode toolchain. Then flip those jobs from `when: manual` to the default.

There is no Windows runner anywhere in the estate and nothing plans one.

## Docs publishing

The config is `zensical.toml` at the repo root: `docs_dir = docs/site` (the
published pages) and `site_dir = docs/.site` (build output, gitignored).
`build-docs.sh` reads `site_dir` back out of the config instead of hard-coding
it, so moving the output cannot silently make CI verify the wrong directory.
The build runs `--strict`, so a broken link or a nav entry pointing at a
missing page fails.

The GitLab `docs-site` job **builds** the site on every push. It does not
publish. Publishing goes to GitHub Pages via
[`../.github/workflows/docs-pages.yml`](../.github/workflows/docs-pages.yml),
which is dormant (`workflow_dispatch` only) because Actions are off for
billing. Both call `ci/build-docs.sh`, so the verified site and the published
site are built by identical code.

Before the first publish, set repository Settings → Pages → Build and
deployment → Source = "GitHub Actions". `site_url` already carries the
`/astar/` project sub-path Pages serves from; a repository rename has to move
with it.

If GitLab Pages is ever preferred instead, rename `docs-site` to `pages` and
publish the output as a `public/` artifact. That is the whole change.

## The decision nobody has made yet

**A self-hosted runner must never be attached to a public repo.** Any pull
request author would get arbitrary code execution on the IONOS build box and,
for the macOS runners, on Rob's personal Mac. Both `gh-runners` READMEs say so
in bold.

This repo is AGPL-3.0, which strongly implies it will be published. GitHub
Pages on a private repo also requires a paid plan, so publishing the docs
points the same way. The moment the repo goes public:

* detach the IONOS fleet and the Mac runner from it, and
* move CI to hosted runners (free for public repos on GitHub; GitLab's free
  shared-runner minutes are limited), or accept no CI.

Keeping it private keeps the free self-hosted CI. That is Rob's call and it has
not been made — nothing in this directory assumes either answer.

## Never in CI, under any trigger

* `IAX_THUMBDV_TESTS=1`, `just dstar-test-hw`, `just ci-hw` — no runner has a
  dongle, and arming it on the Mac would open serial ports on a machine with a
  transmitter interface attached. `astar-sys` defaults to the `dstar` feature,
  so those tests are compiled into a plain `cargo test --workspace`; only the
  env var keeps them asleep. That is why `guard-safety-env.sh` exists.
* `IAX_PORTAL_LIVE=1` with `ASL_USER` / `ASL_PASS` / `ASL_NODE` — hits the real
  AllStarLink portal and would put credentials in a CI environment.
* `IAX_PARROT_LIVE=1` — dials live AllStar node 55553.
* `IAX_THUMBDV_PORT` — a pin that narrows the FTDI scan. It has no purpose in
  CI and no business being set there.
* Anything that runs `astar-server` against a non-loopback bind, or
  `dstar-listen` against a real reflector.

Never transmit on the air autonomously. Rob is the only one who keys on air.
