---
icon: lucide/circle-check
---

# Verifying a build

The repository can check itself. Two recipes cover almost everything, and every
underlying script is runnable by hand so you can reproduce a failure in seconds
rather than reading a pipeline log.

## The two gates

```bash
just ci          # the everyday Rust gate
just ci-full     # everything, including the Swift side (needs full Xcode)
```

| | `just ci` | `just ci-full` |
|---|:--:|:--:|
| `cargo fmt --check` | ✅ | ✅ |
| `cargo clippy -D warnings` | ✅ | ✅ |
| `cargo test --workspace` | ✅ | ✅ |
| C-header drift (`cbindgen`) | ✅ | ✅ |
| The C example builds and links | — | ✅ |
| Python binding smoke test | — | ✅ |
| `swift format lint --strict` | — | ✅ |
| AstarCore unit tests | — | ✅ |

`just ci` is the one to run constantly. `just ci-full` is the gate before
pushing anything under `apps/macos/`, `bindings/swift/` or
`bindings/swift-serial/`, because no CI runner currently covers the Swift side
— it is verified by hand on a Mac.

!!! note "Neither gate builds the app itself"

    `ci-full` runs the *AstarCore* tests and the formatter, not `xcodebuild`.
    If you changed the app, run `just app` too. And the Swift binding tests are
    in neither gate:

    ```bash
    cd bindings/swift-serial && swift test
    ```

## The guards

Four small scripts enforce invariants that a test cannot:

```bash
just guards
```

| Guard | Fails when |
|---|---|
| `guard-safety-env.sh` | Any hardware or live-network opt-in is armed in the environment. |
| `guard-no-git-deps.sh` | A git dependency appears — `ambe-thumbdv` must stay vendored, with its licences. |
| `guard-distribution-claims.sh` | Documentation invents an install channel that does not exist. |
| `guard-spdx-headers.sh` | A first-party `.rs`/`.swift`/`.sh`/`.py` file is missing its `AGPL-3.0-only` SPDX header. |

Each prints one line on success. Run them before a push.

`guard-safety-env.sh` is the one that matters most: `astar-sys` enables the
`dstar` feature by default, so the hardware-touching tests are *compiled into* a
plain `cargo test --workspace`. Only the absence of `IAX_THUMBDV_TESTS=1` keeps
them asleep, and this guard is what makes sure nobody armed it.

## The documentation site

```bash
just docs          # serve at http://localhost:8000 with live reload
just docs-build    # build into docs/.site, exactly as CI does
./ci/build-docs.sh # the same build, the way CI invokes it
```

The build runs `--strict`, so a broken link or a nav entry pointing at a missing
page fails rather than shipping a half-built site. `zensical.toml` at the
repository root is the config — it is TOML, not MkDocs' `mkdocs.yml`.

Only `docs/site/` is published. The rest of `docs/` is internal engineering
material and is deliberately not part of the site.

## Reading a failure

| Failure | What it means | Fix |
|---|---|---|
| `cargo fmt --check` diffs | Formatting drifted | `just fmt` |
| Clippy errors, no code change | Warnings are denied, and a toolchain update added lints | Fix them, or scope the lint — do not silence the recipe |
| `astar.h is out of date` | You changed an `extern "C"` signature or a doc comment on a `#[repr(C)]` type | Run the exact command the check prints, then commit the header |
| `cbindgen: command not found` | Missing optional tool | `cargo install cbindgen --version 0.29.4 --locked` |
| Link error: `-lasound` / `-ludev` | Missing Linux dev packages | [Prerequisites](prerequisites.md#linux) |
| Tests skip with a ThumbDV message | Normal — no dongle attached | Nothing to fix; that is the designed behaviour |
| `swift format` not found | Command Line Tools instead of full Xcode | `sudo xcode-select -s /Applications/Xcode.app` |
| A guard fails on a doc change | You wrote an install claim that is not true | Say "build from source"; there is no tap, cask or download |

## What CI does and does not cover

The pipeline definition is `.gitlab-ci.yml`, and the honest state is that the
Linux half needs a runner that is not registered yet and the macOS half has no
runner at all. `ci/README.md` documents exactly which jobs are real and which
are placeholders.

Until that changes, **`just ci-full` on a Mac is the Swift gate**, and it is
run by hand. Nothing on a server checks the app for you.

## Next steps

* [Building astar](index.md) — back to the overview.
* [The engine](engine.md) · [The macOS app](macos-app.md) · [astar-server](server.md)
