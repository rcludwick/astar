#!/usr/bin/env python3
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
"""Build (and police) the published release manifest.

The site publishes one small static file, ``api/v1/releases.json``, so a
running astar can answer "is there a newer version than me?" without an API,
an account or a request that says anything about the person asking. Design:
``docs/design/version-manifest.md``.

Three modes, all offline and all pure functions of the working tree:

``--write DIR``
    Generate ``DIR/api/v1/releases.json``. ``ci/build-docs.sh`` calls this
    with ``docs/site`` before Zensical runs, and Zensical copies the file
    into the built site verbatim (verified: non-Markdown assets under
    ``docs_dir`` are copied through untouched).

``--check``
    Fail if the version sources disagree, or if ``CHANGELOG.md`` is not in
    descending release order. This is the enforcement half: the lockstep
    between ``apps/macos/project.yml`` and ``zensical.toml`` used to live in
    somebody's head on release day. Wired into ``just ci``, so drift reddens
    the everyday gate rather than the docs deploy.

``--verify-anchors DIR``
    After the site is built, confirm every ``notes_url`` fragment this file
    emitted actually exists as an ``id=`` in the rendered changelog page.
    The anchors are computed from the heading text using Python-Markdown's
    slugify rule; if a toolchain upgrade ever changes that rule, this fails
    the build instead of shipping a manifest full of dead links.

Stdlib only, on purpose. This runs inside a docs build that already has to
work on a bare ``ubuntu-latest`` with ``persist-credentials: false`` — no
network, no token, no dependency worth installing for one JSON file.

The module name uses underscores rather than the hyphens the rest of ``ci/``
prefers because ``ci/test_version_manifest.py`` imports it.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
import unicodedata
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Schema version of the published document. Bump only if an existing field
# changes meaning; adding a field does not (docs/design/version-manifest.md,
# and the same rule CLAUDE.md states for ConfigVersion).
SCHEMA = 1

MANIFEST_PATH = ("api", "v1", "releases.json")

# `## 0.1.9beta — 2026-08-29`. The dash is an em dash in every heading; accept
# any of the three dashes so a typed hyphen is a diff, not a build failure.
HEADING_RE = re.compile(r"^##\s+(?P<version>\S+)\s+[—–-]\s+(?P<date>\d{4}-\d{2}-\d{2})\s*$")

# `    MARKETING_VERSION: "0.1.9beta"` in apps/macos/project.yml. A regex, not
# a YAML parser: this is one scalar at a known key, the key appears exactly
# once as a setting, and adding PyYAML to a docs build to read it would cost
# more than it is worth.
MARKETING_RE = re.compile(r'^\s*MARKETING_VERSION:\s*"(?P<version>[^"]+)"\s*$', re.M)

# The version chip inside zensical.toml's `copyright`, which is raw HTML.
CHIP_RE = re.compile(r'<span class="astar-version">v(?P<version>[^<]+)</span>')

# major.minor.patch, then either the legacy glued suffix (`0.1.9beta`) or
# SemVer's own pre-release (`0.2.0-beta.1`), or nothing (`0.2.0`).
VERSION_RE = re.compile(
    r"^(?P<major>\d+)\.(?P<minor>\d+)\.(?P<patch>\d+)"
    r"(?:-(?P<semver_pre>[0-9A-Za-z.-]+)|(?P<glued_pre>[A-Za-z][0-9A-Za-z]*))?$"
)


class VersionError(ValueError):
    """A version string that does not fit the grammar above."""


# ---------------------------------------------------------------------------
# Version strings
# ---------------------------------------------------------------------------


def normalize(version: str) -> str:
    """Return `version` as strict SemVer.

    astar's released strings are `0.1.9beta`, which is not SemVer — the
    pre-release is glued to the patch number with no separator. SemVer's own
    pre-release syntax (`0.1.9-beta`) would have solved the ordering problem
    from the start; this function is the translation, and it is why leaving
    beta later needs no new code and no schema bump:

        0.1.9beta     -> 0.1.9-beta      (legacy, translated)
        0.2.0-beta.1  -> 0.2.0-beta.1    (already SemVer, unchanged)
        0.2.0         -> 0.2.0           (a real release, unchanged)

    The published manifest carries both: `version` is the exact string the
    app reports, `semver` is this. A client may compare either.
    """
    m = VERSION_RE.match(version)
    if not m:
        raise VersionError(f"not a version astar knows how to order: {version!r}")
    core = f"{int(m['major'])}.{int(m['minor'])}.{int(m['patch'])}"
    pre = m["semver_pre"] or m["glued_pre"]
    return f"{core}-{pre}" if pre else core


def sort_key(version: str) -> tuple:
    """A total order over astar version strings, following SemVer §11.

    The trap this exists for: `0.1.10beta` is NEWER than `0.1.9beta`, and
    string comparison says the opposite. Numbers compare as numbers.

    A version with a pre-release sorts BEFORE the same version without one
    (`0.2.0-beta.1` < `0.2.0`), which is what makes the eventual exit from
    beta order correctly against the betas that preceded it. Pre-release
    identifiers compare field by field: numeric ones numerically and below
    alphanumeric ones, per SemVer.
    """
    m = VERSION_RE.match(version)
    if not m:
        raise VersionError(f"not a version astar knows how to order: {version!r}")
    core = (int(m["major"]), int(m["minor"]), int(m["patch"]))
    pre = m["semver_pre"] or m["glued_pre"]
    if pre is None:
        # 1 sorts after 0: a release outranks every pre-release of itself.
        return (core, 1, ())
    fields = []
    for field in pre.split("."):
        if field.isdigit():
            fields.append((0, int(field), ""))
        else:
            fields.append((1, 0, field))
    return (core, 0, tuple(fields))


def slugify(text: str) -> str:
    """Python-Markdown's default `toc` slug, which is what Zensical renders.

    Verified against a real build: `## 0.1.9beta — 2026-08-29` becomes
    `019beta-2026-08-29`. `--verify-anchors` re-proves it every build rather
    than trusting this comment.
    """
    ascii_text = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode("ascii")
    ascii_text = re.sub(r"[^\w\s-]", "", ascii_text).strip().lower()
    return re.sub(r"[-\s]+", "-", ascii_text)


# ---------------------------------------------------------------------------
# Reading the tree
# ---------------------------------------------------------------------------


def read_changelog(path: Path) -> list[dict]:
    """Every `## <version> — <date>` heading, newest first, as written.

    The changelog is the source of the release list. It is in-repo (no network
    and no token during a docs build), it is already maintained on every
    release, and it is already what the site publishes — so the manifest and
    the release notes it links to can never describe different sets of
    releases. Git tags would need full history at checkout; the GitHub
    Releases API would need network and auth inside a build that deliberately
    has neither.
    """
    releases = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.startswith("## "):
            continue
        m = HEADING_RE.match(line)
        if not m:
            raise SystemExit(
                f"FAIL: {path.name} heading is not '## <version> — <YYYY-MM-DD>':\n  {line}"
            )
        releases.append({"version": m["version"], "date": m["date"], "heading": line[3:].strip()})
    if not releases:
        raise SystemExit(f"FAIL: {path.name} has no '## <version> — <date>' headings")
    return releases


def read_marketing_version(path: Path) -> str:
    m = MARKETING_RE.search(path.read_text(encoding="utf-8"))
    if not m:
        raise SystemExit(f"FAIL: no MARKETING_VERSION setting in {path}")
    return m["version"]


def read_site_config(path: Path) -> dict:
    with path.open("rb") as fh:
        project = tomllib.load(fh).get("project", {})
    copyright_html = project.get("copyright", "")
    chip = CHIP_RE.search(copyright_html)
    if not chip:
        raise SystemExit(
            f"FAIL: no <span class=\"astar-version\">v…</span> chip in {path}'s copyright"
        )
    return {
        "chip_version": chip["version"],
        "site_url": project.get("site_url", "").rstrip("/") + "/",
        "repo_url": project.get("repo_url", "").rstrip("/"),
        "site_dir": project.get("site_dir", "site"),
    }


# ---------------------------------------------------------------------------
# The manifest
# ---------------------------------------------------------------------------


def build_manifest(root: Path = ROOT) -> dict:
    """The published document, as a pure function of the working tree.

    No timestamp: two builds of the same commit publish byte-identical bytes.
    Freshness is `current.date`, which is a fact about the release rather
    than a fact about the build machine's clock.
    """
    config = read_site_config(root / "zensical.toml")
    releases = read_changelog(root / "CHANGELOG.md")
    check_consistency(root, config=config, releases=releases)

    total = len(releases)
    entries = []
    for index, release in enumerate(releases):
        version = release["version"]
        entries.append(
            {
                "version": version,
                "semver": normalize(version),
                # 1 = the first release ever, ascending. A client that finds
                # its own version in this list compares ordinals and needs no
                # version-parsing code at all.
                "ordinal": total - index,
                "date": release["date"],
                "notes_url": f"{config['site_url']}changelog/#{slugify(release['heading'])}",
                "release_url": f"{config['repo_url']}/releases/tag/v{version}",
            }
        )

    return {
        "schema": SCHEMA,
        "product": "astar",
        # Duplicated from releases[0] so the dumbest possible client is one
        # field read away from an answer.
        "current": dict(entries[0]),
        "releases": entries,
    }


def check_consistency(root: Path, config: dict | None = None, releases: list | None = None) -> str:
    """Fail loudly when the version sources disagree. Returns the version.

    Three homes for one number, and they must agree:

      apps/macos/project.yml  MARKETING_VERSION   the app's own idea of itself
      zensical.toml           the version chip    what the site footer claims
      CHANGELOG.md            the newest heading  what the manifest publishes

    Keeping them in step was a manual habit on release day. It is now a gate.

    The Rust workspace `version` in the root Cargo.toml is deliberately NOT in
    this set — see the design doc; it is a Cargo/SemVer artifact that has
    already drifted, and folding it in here would fail the build for a
    pre-existing condition rather than for anything a change did.
    """
    config = config or read_site_config(root / "zensical.toml")
    releases = releases or read_changelog(root / "CHANGELOG.md")

    sources = {
        "apps/macos/project.yml (MARKETING_VERSION)": read_marketing_version(
            root / "apps" / "macos" / "project.yml"
        ),
        "zensical.toml (version chip)": config["chip_version"],
        "CHANGELOG.md (newest heading)": releases[0]["version"],
    }
    if len(set(sources.values())) != 1:
        lines = "\n".join(f"    {value:<16} {name}" for name, value in sources.items())
        raise SystemExit(
            "FAIL: the version is spelled differently in different places.\n"
            f"{lines}\n"
            "      All three must carry the same string. Bump them together, or\n"
            "      add the missing CHANGELOG.md heading for the release."
        )

    # Every string must be orderable, and the changelog must already be in the
    # order it claims (newest first). A heading filed in the wrong place would
    # otherwise publish a wrong `current` and a wrong set of ordinals.
    for release in releases:
        sort_key(release["version"])
    versions = [r["version"] for r in releases]
    if len(set(versions)) != len(versions):
        dupes = sorted({v for v in versions if versions.count(v) > 1})
        raise SystemExit(f"FAIL: CHANGELOG.md names {', '.join(dupes)} more than once")
    expected = sorted(versions, key=sort_key, reverse=True)
    if versions != expected:
        raise SystemExit(
            "FAIL: CHANGELOG.md is not in descending version order.\n"
            f"    found:    {', '.join(versions)}\n"
            f"    expected: {', '.join(expected)}"
        )
    return releases[0]["version"]


def write_manifest(docs_dir: Path, root: Path = ROOT) -> Path:
    manifest = build_manifest(root)
    out = docs_dir.joinpath(*MANIFEST_PATH)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return out


def verify_anchors(site_dir: Path, root: Path = ROOT) -> int:
    """Prove every `notes_url` fragment exists in the built changelog page."""
    page = site_dir / "changelog" / "index.html"
    if not page.is_file():
        raise SystemExit(f"FAIL: no built changelog page at {page}")
    html = page.read_text(encoding="utf-8")
    manifest = build_manifest(root)
    missing = [
        entry["version"]
        for entry in manifest["releases"]
        if f'id="{entry["notes_url"].split("#", 1)[1]}"' not in html
    ]
    if missing:
        raise SystemExit(
            "FAIL: the changelog page has no anchor for: " + ", ".join(missing) + "\n"
            "      The heading slug rule changed under us. Fix slugify() in\n"
            "      ci/version_manifest.py to match what the site now renders."
        )
    return len(manifest["releases"])


def main(argv: list[str] | None = None) -> int:
    # A literal, not __doc__: module docstrings are Optional (and are stripped
    # entirely under `python -OO`), so reading one here is a latent None.
    parser = argparse.ArgumentParser(description="Build and police the published release manifest.")
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--write", metavar="DOCS_DIR", help="write DOCS_DIR/api/v1/releases.json")
    group.add_argument("--check", action="store_true", help="fail if the version sources disagree")
    group.add_argument(
        "--verify-anchors", metavar="SITE_DIR", help="check the emitted changelog anchors resolve"
    )
    args = parser.parse_args(argv)

    if args.check:
        version = check_consistency(ROOT)
        count = len(read_changelog(ROOT / "CHANGELOG.md"))
        print(f"version-manifest: v{version} agrees across 3 sources · {count} releases in order")
        return 0

    if args.verify_anchors:
        count = verify_anchors(Path(args.verify_anchors), ROOT)
        print(f"version-manifest: {count} changelog anchors resolve")
        return 0

    out = write_manifest(Path(args.write), ROOT)
    manifest = json.loads(out.read_text(encoding="utf-8"))
    print(
        f"version-manifest: wrote {out.relative_to(ROOT)} "
        f"(schema {manifest['schema']}, current v{manifest['current']['version']}, "
        f"{len(manifest['releases'])} releases)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
