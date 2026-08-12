---
icon: lucide/apple
---

# The macOS app

The SwiftUI menu-bar client. This is the front-end that gets the design
attention, and the one with the most prerequisites — a full Xcode, XcodeGen,
and one step a fresh checkout must not skip.

Make sure [Prerequisites](prerequisites.md#macos) is satisfied first,
especially `xcode-select -p` pointing at a real Xcode.

## The whole build

```bash
git clone https://github.com/rcludwick/astar
cd astar

just xcframework   # (1)!
just run           # (2)!
```

1.  Builds **both** Swift xcframeworks — `AstarStation` (the engine) and
    `AstarSerial` (PTT). This is the step to not skip.
2.  `xcodegen generate` → `xcodebuild` → launch.

Then look for the **rainbow asterisk** in the menu bar. astar has no Dock icon
and no main window; left-click the asterisk to open the dial popover. The
running version is in the popover footer, so you can always tell what you are
actually running.

## Why `just xcframework` is mandatory

The Swift bindings link the Rust engine through two xcframeworks:

| Framework | Built from | Lands at |
|---|---|---|
| `astar.xcframework` | `crates/astar-sys` | `bindings/swift/` |
| `astarserial.xcframework` | `crates/astar-serial-sys` | `bindings/swift-serial/` |

They are large and completely regenerable, so they are **gitignored** — which
means a fresh clone does not have them and cannot build the app until you make
them. `just xcframework` runs both build scripts:

```bash
./bindings/swift/build-xcframework.sh
./bindings/swift-serial/build-xcframework.sh
```

!!! warning "Always run both — a stale cache lies"

    Running one script and not the other leaves you with one current framework
    and one from whenever you last built it. The symptoms are bewildering:
    linker errors about symbols that plainly exist, or worse, a build that
    succeeds against an old ABI and misbehaves at run time.

    `just xcframework` is the recipe precisely so that "both" is the default.

`just app` and `just run` **hard-fail with instructions** when a framework is
missing, rather than silently falling back. `bindings/swift/Package.swift`
picks its linking path by whether the xcframework is on disk — a `binaryTarget`
if present, a `systemLibrary` plus raw `.a` if not — and the app's build
scripts refuse the second path on purpose.

Rebuild the xcframeworks whenever you change anything in `crates/`. Nothing
detects that for you.

## The Xcode project is generated

`apps/macos/astar.xcodeproj` is **not** in git. It is generated from
`apps/macos/project.yml` by XcodeGen, which is why the prerequisite exists:

```bash
just generate     # xcodegen generate, on its own
```

`just app` and `just run` do this for you. Edit `project.yml`, never the
`.xcodeproj` — changes to the generated project are lost on the next build.

The project points straight at `bindings/swift` and `bindings/swift-serial`.
There is no vendoring step and no sibling repository: this is one tree.

## Everyday recipes

```bash
just run                 # build if needed, then launch
just run --no-build      # relaunch what is already built
just app                 # build only
just app --release       # release configuration
just app --clean         # clean build
just app-test            # AstarCore unit tests (plain SwiftPM, no Xcode project)
```

`just app-test` runs the tests in `apps/macos/Packages/AstarCore`, which is a
normal SwiftPM package and needs no generated project. It is the fast inner
loop for logic changes.

The Swift bindings have their own tests, and they are **not** part of
`just ci-full`:

```bash
cd bindings/swift-serial && swift test
```

## Formatting

Swift sources are formatted with `swift format`, which ships inside Xcode and
reads `.swift-format` at the repository root:

```bash
just swift-fmt           # rewrite in place
just swift-fmt-check     # report only; --strict, fails on any finding
```

`swift-fmt-check` is part of `just ci-full`, so run it before pushing anything
under `apps/macos/`.

## A local disk image

```bash
just dmg          # → apps/macos/build/astar.dmg
```

!!! danger "This is not a release"

    The image is **unsigned and un-notarized**. Gatekeeper will refuse it on
    any Mac that did not build it. It exists so you can move the app to another
    machine you own — it is not a distribution channel, and astar publishes no
    binaries anywhere.

## Installing your build

There is no installer. Copy the built app wherever you keep applications:

```bash
ditto apps/macos/build/DD/Build/Products/Debug/astar.app /Applications/astar.app
```

Re-run that after a rebuild if you launch astar from `/Applications` rather
than from `just run`.

## iOS

`project.yml` already builds a multiplatform target, and the xcframeworks carry
iOS slices when `rustup` has the iOS targets installed. No iOS UI work has
happened — the target builds, and that is the whole claim.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `xcodebuild: error: tool 'xcodebuild' requires Xcode` | Command Line Tools selected instead of Xcode | `sudo xcode-select -s /Applications/Xcode.app` |
| Build stops saying an xcframework is missing | Fresh checkout, or you deleted `target/` | `just xcframework` |
| Undefined symbols that clearly exist in the Rust source | One xcframework is stale | `just xcframework` — both, again |
| `xcodegen: command not found` | Missing prerequisite | `brew install xcodegen` |
| The app builds but shows no menu-bar icon | It launched — there is no Dock icon by design | Look for the rainbow asterisk; check other menu-bar items are not crowding it out |
| No M17 in the network picker | No system `libcodec2` | [Codec 2](prerequisites.md#codec-2-only-for-m17) |
| No D-Star anywhere in the UI | Not wired into the GUI yet | Expected — use `astar-cli`, see [Hardware](../macos/hardware.md) |

## Next steps

* [Hardware](../macos/hardware.md) — USB radio interfaces and PTT wiring.
* [Using astar](../macos/usage.md) — day-to-day operating.
* [Verifying a build](verifying.md) — the full gate.
