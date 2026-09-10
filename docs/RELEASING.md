# Releasing astar

Two commands, plus one thing you write by hand first.

```console
$ just release 0.1.13-beta --dry-run   # read-only: prints everything it would do
$ just release 0.1.13-beta             # bump, gates, commit, tag, push to ORIGIN
$ just publish 0.1.13-beta             # ONLY when Rob says to publish
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

5. **Commit, tag, push**: `chore: <version>`, `v<version>`, both to `origin`.

## `just publish <version>`

Preconditions: the tag exists locally *and* on `origin`, `public/main` is an
ancestor of `HEAD` (both repos share one history, so publishing is a
fast-forward), `apps/macos/build/astar.dmg` exists and passes the same two
assertions `make-dmg.sh` ends with — `spctl --assess --type open` and a stapled
ticket from `xcrun stapler validate` — and `gh auth status` is happy.

Then it pushes `main` and the tag to `public`, and creates the release with
`gh release create`, with:

* the notes lifted verbatim from this version's `CHANGELOG.md` section (heading
  stripped, `Full changelog: https://rcludwick.github.io/astar/changelog/`
  appended, the way every earlier release carried it);
* `astar.dmg` attached **under exactly that name** — the docs link to the asset
  by name;
* `--latest`, never `--prerelease`. The docs' "latest release" link has to
  resolve to it, and a prerelease does not answer there.

Afterwards it reads `repos/rcludwick/astar/releases/latest` back and checks the
tag, rather than assuming the flag took.

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

If a failure happens *after* the commit and tag but before the push, the tag is
local only: `git tag -d v<version>` and `git reset --hard HEAD~1` put you back.

## Testing the release script

`just release-test` builds a throwaway repo in a temp directory and drives
`ci/release.sh` against it — the changelog refusal, the dry run, and the bump
proving it rewrites the five homes and nothing else (there is a decoy
third-party dependency at the same version to catch a careless
search-and-replace). It calls no cargo, no just, no gh and no network, takes
about a second, and runs as part of `just ci`.
