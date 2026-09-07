#!/usr/bin/env python3
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
"""Generate the landing page for the internal design-docs site.

`just design` runs this before `zensical serve`. It writes
docs/superpowers/index.md — a dated index of every spec, plan and note — so
the site has a home page and the documents are reachable in date order rather
than only through the derived sidebar.

The output is generated, never hand-edited: it is rewritten on every run, and
docs/superpowers/ is gitignored in full, so nothing here is ever committed.
"""

from __future__ import annotations

import re
import sys
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs" / "superpowers"

# Directory -> (heading, one-line description). Order is the page order.
SECTIONS = [
    ("specs", "Specs", "Designs agreed before implementation."),
    ("plans", "Plans", "Task-by-task implementation plans, with checkboxes."),
    ("notes", "Notes", "Findings that belong to no single spec."),
    (
        "implemented",
        "Implemented",
        "Shipped. Kept for the reasoning, not as a to-do list.",
    ),
]

DATED = re.compile(r"^(\d{4}-\d{2}-\d{2})-(.+)\.md$")


def title_of(path: Path, slug: str) -> str:
    """Prefer the document's own H1; fall back to its filename slug."""
    try:
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("# "):
                return line[2:].strip()
    except OSError:
        pass
    return slug.replace("-", " ")


def entries(directory: Path):
    rows = []
    for md in sorted(directory.glob("*.md")):
        if md.name == "index.md":
            continue
        m = DATED.match(md.name)
        when, slug = (m.group(1), m.group(2)) if m else ("", md.stem)
        rows.append((when, title_of(md, slug), f"{directory.name}/{md.name}"))
    # Newest first; undated entries sort last.
    rows.sort(key=lambda r: (r[0] == "", r[0]), reverse=True)
    return rows


def main() -> int:
    if not DOCS.is_dir():
        print(f"no design docs at {DOCS}", file=sys.stderr)
        return 1

    out = [
        "# astar — design docs",
        "",
        "Internal specs, plans and notes. **This site is not published** —"
        " `docs/superpowers/` is gitignored and `ci/build-docs.sh` builds only"
        " `zensical.toml`, never this one.",
        "",
        "A document's section is a claim about the state of the code: **Specs**"
        " and **Plans** are open work, **Implemented** has shipped. That claim"
        " rots unless someone moves the file, so check an open design against"
        " the code and `docs/BACKLOG.md` before trusting it.",
        "",
    ]

    total = 0
    for name, heading, blurb in SECTIONS:
        directory = DOCS / name
        if not directory.is_dir():
            continue
        rows = entries(directory)
        if not rows:
            continue
        total += len(rows)
        out += [f"## {heading}", "", f"{blurb} &nbsp;·&nbsp; {len(rows)} documents", ""]
        out += ["| Date | Document |", "| --- | --- |"]
        out += [f"| {when or '—'} | [{title}]({link}) |" for when, title, link in rows]
        out.append("")

    out.append("---")
    out.append("")
    out.append(f"*{total} documents · regenerated {date.today().isoformat()} by `just design`.*")

    (DOCS / "index.md").write_text("\n".join(out) + "\n", encoding="utf-8")
    print(f"design index: {total} documents -> {DOCS / 'index.md'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
