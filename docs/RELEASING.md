# Releasing astar

Two commands, plus one thing you write by hand first.

```console
$ just release 0.1.13-beta --dry-run   # read-only: prints everything it would do
$ just release 0.1.13-beta             # bump, gates, commit, tag, push to ORIGIN
$ just publish 0.1.13-beta             # ONLY when Rob says to publish; cleans up after
```

`release` goes as far as the private repo and stops. `publish` is the second,
deliberate step: it pushes to the public repo and creates the GitHub release.
Nothing runs `publish` on Rob's behalf — CLAUDE.md is explicit that publishing
is his call and never a step in a task.

## Before you start: the release notes

The notes are written by a person, not generated. Put a new section at the top
of `CHANGELOG.md`:

```markdown
## 0.1.13-beta — 2026-09-09
```

Bullets that have been accumulating live in
`docs/superpowers/notes/2026-09-07-pending-changelog.md` (gitignored, and it may
not exist) — move them into the section, edit them into prose, and commit.

`just release` refuses to do anything until that heading names the exact version
you asked for, because the section is what the GitHub release body is lifted
from and what `ci/version_manifest.py` publishes as the release list.

## `just release <version>`

1. **Preconditions.** Each fails fast, with one line saying why.

   | Check | Why |
   | --- | --- |
   | on `main` | releases are cut from the merged trunk, not a work branch |
   | clean tree | the bump commit must contain the bump and nothing else |
   | `git fetch origin`, `main` not behind `origin/main` | otherwise the tag names a commit that is not what is published |
   | tag `v<version>` does not exist | that release was already cut |
   | `<version>` is SemVer | since `0.1.10-beta`; the glued `0.1.13beta` form sorts wrongly and is rejected |
   | `CHANGELOG.md`'s newest heading is `<version>` | see above |

2. **The version bump**, in all five homes at once: `Cargo.toml`'s
   `[workspace.package] version`, every `astar-*` path dependency's `version =`
   under `crates/*/` and `apps/*/`, `apps/macos/project.yml`'s
   `MARKETING_VERSION`, and `zensical.toml`'s footer chip. Then `cargo update -w`
   for `Cargo.lock`, and `python3 ci/version_manifest.py --check` to prove all
   five agree. The path deps are the ones that quietly rot: Cargo reads a stale
   `version = "0.1.3-beta"` as a caret range the new crate still satisfies, so
   nothing ever complains.

3. **`just reflectors`** — refreshes the bundled reflector snapshot, which is
   what a first launch with no network reads. Nothing else ever refreshes it, so
   release time is the only time it happens. `--skip-reflectors` skips it (it is
   the only step that needs the network).

4. **The gates**: `just ci`, `just xcframework`, `just app-test`, `just dmg`.
   The DMG lands at `apps/macos/build/astar.dmg`, signed and notarized when the
   Developer ID identity and the `astar-notary` keychain profile are on the
   machine — `make-dmg.sh` prints which of ad-hoc / signed / signed+notarized
   you actually got, and only the last is fit to publish.

5. **Commit, tag, push**: `chore: <version>`, `v<version>`, both to `origin`,
   with `git push --atomic` so the branch and the tag land together or not at
   all.

There is one more flag, `--bump-only`: it rewrites the version strings and
stops — no cargo, no manifest check, **no gates at all**, no git. It exists so
`ci/test_release_sh.sh` can drive the bump in isolation. It is not a way to
release, and a tree bumped with it has been proven by nothing.

## The server artefact

astar-server ships as one static executable per architecture, built by
`just server-release x86_64-unknown-linux-musl` (and `aarch64-unknown-linux-musl`).
The recipe runs `ci/check-server-release.sh`, which must pass: the binary is
statically linked, carries the listener page and LAME, and its `--features`
are exactly the `release` set (`dstar-soft-ambe` included). `just release` does not build it yet; attach
it to a GitHub release by hand when Rob publishes one. It statically links
LGPL code (LAME, Codec 2, the software D-Star vocoder), so the release notes point at
`LICENSE-EXCEPTIONS.md` and the matching source tag.

## The net recorder artefact

`astar-netrec` is built on its own, not by `just release`:

```bash
cargo build --release -p astar-netrec --features release
```

`--features release` is the set that includes Codec 2 and the software D-Star
vocoder (`ci/guard-codec2-licensing.sh` and `ci/guard-dstar-soft-ambe.sh`
check that the set is pulled in and that a plain build is not). It is not
cross-built or attached to a release by a recipe yet; like the server binary,
attach it to a GitHub release by hand when Rob publishes one. It statically
links LGPL code (Codec 2, the software D-Star vocoder), so the release notes
point at `LICENSE-EXCEPTIONS.md` and the matching source tag. The systemd unit
and launchd agent are in `docs/site/netrec/`. A recipe for a static VPS build is
`astar-netrec-release` in `docs/BACKLOG.md`.

## `just publish <version>`

Preconditions:

* you are on `main` — `main` is the branch that gets pushed, so `main` is what
  every other check is about;
* the tag exists locally, is reachable from `main`, and is on `origin`;
* the tag is not already on `public`, and public's `main` doesn't already
  carry exactly the tag's files;
* it is not Monday–Friday between 7 AM and 5 PM MST (UTC−7 all year). No
  push goes to the public repo in those hours (Rob's rule). A dry run only
  says whether a push would be allowed right now;
* `CHANGELOG.md`'s **newest** heading is the version being published. Anything
  below the top is a release that is already out, and publishing its notes as
  `--latest` would put stale notes on the release the docs link to;
* `apps/macos/build/astar.dmg` exists and passes **both** assertions
  `make-dmg.sh` ends with — `spctl --assess --type open` *and* a stapled ticket
  from `xcrun stapler validate`. Neither implies the other: Apple can notarize
  an image whose ticket never got stapled, and that image passes `spctl` on a
  machine that can reach Apple and fails on one that cannot. `make-dmg.sh`
  exits 3 rather than produce a DMG missing either, so publishing one would be
  shipping an artifact that script already refused;
* `gh auth status` is happy.

Then it adds **one squashed commit** to `public`, and creates the release
with `gh release create`.

The public repo has its own history, which shares no commit with the private
one (Rob, 2026-11-01: "Private should stay private always"). Public `main` is
one commit per release, named `astar <version>`:

* it carries exactly the files of the release tag, so the public code is the
  release, byte for byte;
* it sits on top of the previous release's public commit, so the push is a
  plain fast-forward on public;
* it is dated at a random evening time, 18:00 to 22:59 MST, on the day the
  release was tagged, and never before the commit it sits on;
* its tag on public is a separate tag object with the same name and date,
  pointing at the squashed commit. The private tag of the same name is left
  alone.

The commit and the tag go up in one atomic push. No private commit object
ever reaches public. This clone's `public` remote fetches `main` only and no
tags (`remote.public.tagOpt=--no-tags`), because public's tags name different
commits from the private tags of the same name, and fetching them would
overwrite the private ones. The old shared history was squashed this way on
2026-11-01, and a bundle of it is kept at
`/Volumes/data/backups/astar-public-pre-squash-2026-11-01.bundle` on Rob's Mac.

The release itself is created with:

* the notes lifted verbatim from this version's `CHANGELOG.md` section (heading
  stripped, `Full changelog: https://rcludwick.github.io/astar/changelog/`
  appended, the way every earlier release carried it);
* `astar.dmg` attached **under exactly that name** — the docs link to the asset
  by name;
* `--latest`, never `--prerelease`. The docs' "latest release" link has to
  resolve to it, and a prerelease does not answer there.

Afterwards it reads `repos/rcludwick/astar/releases/latest` back and checks the
tag, rather than assuming the flag took.

## Cleaning up afterwards

Once GitHub confirms the release is live, `just publish` runs
`ci/clean-builds.sh --after-publish`. It removes the build output the release
needed and nothing needs any more: the host Rust builds (`target/debug`,
`target/release`, the Intel and Linux cross builds), the xcframework slices'
incremental caches, the Release app, the DMGs of older releases, the DMG just
uploaded, and any worktree that is clean and merged into `main` (with its
branch). A release's gates leave 20–30 GB behind, and nothing else prunes it.

It keeps the Debug app (`just run` launches it, and it may be running), the
xcframeworks, `target/server-release`, and every worktree that is dirty or
unmerged. While cargo, rustc or xcodebuild is running it leaves `target/` and
the app build alone and says so. A failure here never undoes the publish; it
prints `just clean-builds` to run again.

`just clean-builds` runs the same clean by hand at any time (without
`--after-publish`, so it keeps `astar.dmg`), and `--dry-run` lists what it
would remove. The next `just ci` after a clean rebuilds from scratch, which
takes a few minutes longer than usual.

When a checkout's build folders are linked to another drive
(`just link-builds`), the clean works through the links, removes a merged
worktree's linked folders along with it, and sweeps linked folders whose
worktree no longer exists.

## When a gate fails

The script never resets your tree behind your back. If something fails *after*
the version edits are made, it says exactly which files are modified and prints
the command to undo them:

```console
$ git checkout -- Cargo.toml Cargo.lock apps/macos/project.yml zensical.toml crates/…/Cargo.toml
```

Nothing is committed, tagged or pushed before all four gates are green, so a
failure never leaves a half-released repo — only a modified working tree. Fix
what broke, undo the edits (the script refuses a dirty tree, so you must), and
run `just release <version>` again from the top.

The commit, the tag and the push each have their own failure message naming
exactly what exists locally at that point. The push is the one that really
fails in practice — the gates take twenty minutes, which is long enough for
`origin/main` to move — and it is `--atomic`, so main and the tag either both
landed or neither did. Either rebase and re-push:

```console
$ git pull --rebase origin main && git push --atomic origin main v<version>
```

...re-running the gates first if the rebase pulled in real work, or unwind and
start over: `git tag -d v<version> && git reset --hard HEAD~1`.

## Testing the release script

`just release-test` runs both suites against throwaway repos in a temp
directory, and runs as part of `just ci`.

`ci/test_release_sh.sh` drives `ci/release.sh`: the changelog refusal, the dry
run, and the bump proving it rewrites the five homes and nothing else (there is
a decoy third-party dependency at the same version to catch a careless
search-and-replace).

`ci/test_publish_sh.sh` drives `ci/publish.sh` with `gh`, `spctl` and `stapler`
stubbed on `PATH` and bare repos standing in for `origin` and `public`, so the
refusals that protect a published release are provable offline: the
newest-heading rule, the spctl-AND-stapler gate in both directions, publishing
from the wrong branch, a diverged public repo, and a dry run that prints the
three commands and runs none of them.

Neither calls cargo, just, gh or the network, neither pushes anything, and
together they take about two seconds.
