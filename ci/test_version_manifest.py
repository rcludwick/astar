#!/usr/bin/env python3
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
"""Tests for the published release manifest generator.

Run::

    python3 ci/test_version_manifest.py

Stdlib only and no test runner, matching bindings/python/test_smoke.py: this
has to run inside a docs build on a bare ubuntu-latest.

The interesting surface is ordering. `0.1.10beta` is newer than `0.1.9beta`
and string comparison says the opposite, which is the bug this file exists to
prevent; the rest pins the drift gate and the changelog-anchor slug.
"""

import json
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import version_manifest as vm  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent


# ---------------------------------------------------------------------------
# Ordering
# ---------------------------------------------------------------------------


def test_double_digit_patch_outranks_single_digit() -> None:
    """The trap. Lexicographically '0.1.10beta' < '0.1.9beta'; it must not be."""
    assert vm.sort_key("0.1.10beta") > vm.sort_key("0.1.9beta")
    assert "0.1.10beta" < "0.1.9beta", "string order is the wrong answer, as advertised"

    shuffled = ["0.1.9beta", "0.1.10beta", "0.1.2beta", "0.1.11beta", "0.2.0beta"]
    assert sorted(shuffled, key=vm.sort_key) == [
        "0.1.2beta",
        "0.1.9beta",
        "0.1.10beta",
        "0.1.11beta",
        "0.2.0beta",
    ]


def test_double_digit_minor_and_major() -> None:
    assert vm.sort_key("0.10.0beta") > vm.sort_key("0.9.9beta")
    assert vm.sort_key("10.0.0") > vm.sort_key("9.99.99")


def test_release_outranks_its_own_prereleases() -> None:
    """SemVer §11: 0.2.0-beta.1 < 0.2.0. This is what makes leaving beta work."""
    assert vm.sort_key("0.2.0") > vm.sort_key("0.2.0-beta.1")
    assert vm.sort_key("0.2.0") > vm.sort_key("0.2.0beta")
    # ...but a release never outranks a LATER pre-release.
    assert vm.sort_key("0.2.1-beta.1") > vm.sort_key("0.2.0")


def test_prerelease_identifiers_compare_per_semver() -> None:
    assert vm.sort_key("0.2.0-beta.2") > vm.sort_key("0.2.0-beta.1")
    # Numeric identifiers compare numerically, not as strings.
    assert vm.sort_key("0.2.0-beta.10") > vm.sort_key("0.2.0-beta.9")
    # Numeric identifiers rank below alphanumeric ones.
    assert vm.sort_key("0.2.0-rc") > vm.sort_key("0.2.0-1")
    assert vm.sort_key("0.2.0-rc.1") > vm.sort_key("0.2.0-beta.1")


def test_legacy_and_semver_shapes_interleave() -> None:
    """The transition out of beta needs no code change: both shapes sort together."""
    mixed = ["0.1.9beta", "0.2.0-beta.1", "0.2.0", "0.1.10beta"]
    assert sorted(mixed, key=vm.sort_key) == [
        "0.1.9beta",
        "0.1.10beta",
        "0.2.0-beta.1",
        "0.2.0",
    ]


def test_equal_versions_are_equal() -> None:
    assert vm.sort_key("0.1.9beta") == vm.sort_key("0.1.9beta")
    assert not vm.sort_key("0.1.9beta") > vm.sort_key("0.1.9beta")


def test_unparseable_versions_raise() -> None:
    for bad in ("0.1", "v0.1.9beta", "0.1.9.4beta", "", "beta", "0.1.x"):
        try:
            vm.sort_key(bad)
        except vm.VersionError:
            continue
        raise AssertionError(f"{bad!r} should not parse as a version")


# ---------------------------------------------------------------------------
# Normalisation
# ---------------------------------------------------------------------------


def test_normalize_translates_the_glued_suffix() -> None:
    assert vm.normalize("0.1.9beta") == "0.1.9-beta"
    assert vm.normalize("0.1.10beta") == "0.1.10-beta"


def test_normalize_leaves_real_semver_alone() -> None:
    assert vm.normalize("0.2.0") == "0.2.0"
    assert vm.normalize("0.2.0-beta.1") == "0.2.0-beta.1"
    assert vm.normalize("1.0.0-rc.2") == "1.0.0-rc.2"


def test_normalized_strings_sort_the_same_as_the_originals() -> None:
    versions = ["0.1.9beta", "0.1.10beta", "0.2.0-beta.1", "0.2.0"]
    assert [vm.normalize(v) for v in sorted(versions, key=vm.sort_key)] == sorted(
        (vm.normalize(v) for v in versions), key=vm.sort_key
    )


# ---------------------------------------------------------------------------
# The changelog anchor
# ---------------------------------------------------------------------------


def test_slugify_matches_the_rendered_heading_id() -> None:
    """Pinned against a real Zensical build of docs/site/changelog.md."""
    assert vm.slugify("0.1.9beta — 2026-08-29") == "019beta-2026-08-29"
    assert vm.slugify("0.1.10beta — 2026-09-01") == "0110beta-2026-09-01"
    # A plain hyphen instead of an em dash renders the same slug.
    assert vm.slugify("0.2.0 - 2027-01-01") == "020-2027-01-01"


# ---------------------------------------------------------------------------
# The drift gate
# ---------------------------------------------------------------------------


def _fake_tree(marketing: str, chip: str, changelog: str) -> Path:
    root = Path(tempfile.mkdtemp(prefix="astar-version-manifest-"))
    (root / "apps" / "macos").mkdir(parents=True)
    (root / "apps" / "macos" / "project.yml").write_text(
        f'settings:\n  base:\n    MARKETING_VERSION: "{marketing}"\n', encoding="utf-8"
    )
    (root / "zensical.toml").write_text(
        "[project]\n"
        'site_url = "https://rcludwick.github.io/astar/"\n'
        'repo_url = "https://github.com/rcludwick/astar"\n'
        'site_dir = "docs/.site"\n'
        f'copyright = "AGPL-3.0-only <span class=\\"astar-version\\">v{chip}</span>"\n',
        encoding="utf-8",
    )
    (root / "CHANGELOG.md").write_text(changelog, encoding="utf-8")
    return root


TWO_RELEASES = "# Changelog\n\n## 0.1.10beta — 2026-09-01\n\nx\n\n## 0.1.9beta — 2026-08-29\n\ny\n"


def test_agreeing_sources_pass() -> None:
    root = _fake_tree("0.1.10beta", "0.1.10beta", TWO_RELEASES)
    assert vm.check_consistency(root) == "0.1.10beta"


def test_a_stale_site_chip_fails() -> None:
    root = _fake_tree("0.1.10beta", "0.1.9beta", TWO_RELEASES)
    try:
        vm.check_consistency(root)
    except SystemExit as exit_:
        assert "spelled differently" in str(exit_), exit_
        assert "zensical.toml" in str(exit_)
    else:
        raise AssertionError("a stale version chip should fail the gate")


def test_a_missing_changelog_entry_fails() -> None:
    """The release-day case: project.yml bumped, no changelog heading written."""
    root = _fake_tree("0.1.11beta", "0.1.11beta", TWO_RELEASES)
    try:
        vm.check_consistency(root)
    except SystemExit as exit_:
        assert "CHANGELOG.md" in str(exit_)
    else:
        raise AssertionError("an unwritten changelog entry should fail the gate")


def test_a_misfiled_changelog_heading_fails() -> None:
    out_of_order = "# Changelog\n\n## 0.1.9beta — 2026-08-29\n\nx\n\n## 0.1.10beta — 2026-09-01\n\ny\n"
    root = _fake_tree("0.1.9beta", "0.1.9beta", out_of_order)
    try:
        vm.check_consistency(root)
    except SystemExit as exit_:
        assert "descending version order" in str(exit_), exit_
    else:
        raise AssertionError("a changelog out of order should fail the gate")


def test_a_repeated_version_fails() -> None:
    dupe = "# Changelog\n\n## 0.1.9beta — 2026-08-29\n\nx\n\n## 0.1.9beta — 2026-08-28\n\ny\n"
    root = _fake_tree("0.1.9beta", "0.1.9beta", dupe)
    try:
        vm.check_consistency(root)
    except SystemExit as exit_:
        assert "more than once" in str(exit_), exit_
    else:
        raise AssertionError("a version named twice should fail the gate")


def test_a_malformed_heading_fails() -> None:
    bad = "# Changelog\n\n## 0.1.9beta (2026-08-29)\n\nx\n"
    root = _fake_tree("0.1.9beta", "0.1.9beta", bad)
    try:
        vm.check_consistency(root)
    except SystemExit as exit_:
        assert "heading is not" in str(exit_), exit_
    else:
        raise AssertionError("a heading the manifest cannot read should fail the gate")


# ---------------------------------------------------------------------------
# The document itself
# ---------------------------------------------------------------------------


def test_manifest_shape() -> None:
    root = _fake_tree("0.1.10beta", "0.1.10beta", TWO_RELEASES)
    manifest = vm.build_manifest(root)

    assert manifest["schema"] == vm.SCHEMA
    assert manifest["product"] == "astar"
    assert manifest["current"] == manifest["releases"][0]
    assert [r["version"] for r in manifest["releases"]] == ["0.1.10beta", "0.1.9beta"]
    # Oldest release is ordinal 1 and they ascend with the release order.
    assert [r["ordinal"] for r in manifest["releases"]] == [2, 1]
    newest = manifest["releases"][0]
    assert newest["semver"] == "0.1.10-beta"
    assert newest["date"] == "2026-09-01"
    assert newest["notes_url"] == (
        "https://rcludwick.github.io/astar/changelog/#0110beta-2026-09-01"
    )
    assert newest["release_url"] == "https://github.com/rcludwick/astar/releases/tag/v0.1.10beta"


def test_manifest_carries_no_identifiers_or_timestamps() -> None:
    """It is fetched by users' machines; it must not invite a tracking design.

    No build timestamp either: the document is a pure function of the tree, so
    the same commit always publishes the same bytes.
    """
    root = _fake_tree("0.1.10beta", "0.1.10beta", TWO_RELEASES)
    blob = json.dumps(vm.build_manifest(root))
    for forbidden in ("generated", "timestamp", "uuid", "token", "?", "client"):
        assert forbidden not in blob, f"manifest should not carry {forbidden!r}"


def test_write_lands_at_the_published_path() -> None:
    root = _fake_tree("0.1.10beta", "0.1.10beta", TWO_RELEASES)
    docs_dir = root / "docs" / "site"
    docs_dir.mkdir(parents=True)
    out = vm.write_manifest(docs_dir, root)
    assert out == docs_dir / "api" / "v1" / "releases.json"
    assert json.loads(out.read_text(encoding="utf-8"))["current"]["version"] == "0.1.10beta"


def test_writing_twice_is_byte_identical() -> None:
    root = _fake_tree("0.1.10beta", "0.1.10beta", TWO_RELEASES)
    docs_dir = root / "docs" / "site"
    docs_dir.mkdir(parents=True)
    first = vm.write_manifest(docs_dir, root).read_bytes()
    second = vm.write_manifest(docs_dir, root).read_bytes()
    assert first == second


def test_this_repository_is_consistent() -> None:
    """The real tree, not a fixture: `just ci` runs exactly this check."""
    version = vm.check_consistency(ROOT)
    manifest = vm.build_manifest(ROOT)
    assert manifest["current"]["version"] == version
    assert manifest["releases"][-1]["ordinal"] == 1


def main() -> int:
    tests = [value for name, value in sorted(globals().items()) if name.startswith("test_")]
    for test in tests:
        test()
        print(f"ok: {test.__name__}")
    print(f"\nall {len(tests)} version-manifest tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
