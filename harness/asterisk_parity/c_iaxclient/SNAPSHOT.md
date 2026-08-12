# libiax2 source snapshot

This directory mirrors `vendor/iaxclient/lib/libiax2` from astar at a pinned SHA, captured via
`git archive` (always pristine — never reflects astar's working tree).
Refresh with `./snapshot.sh` and commit the result.

| Field | Value |
|-------|-------|
| Astar repo HEAD | `10d45f438d82039ad7e79c280a8b5859473acba9` |
| Last commit touching libiax2 | `3613bc9474112c7ef348aa52c2027f07b4b7215b` |
| Snapshot source | `/Users/rob/dev/astar/vendor/iaxclient/lib/libiax2` at HEAD |
| Snapshot taken by | `rob@Robs-Mac-mini.local` |

Patches applied on top of this snapshot (in order) live under
`./patches/`. The Dockerfile unpacks `vendored/`, then `git apply` each
`.patch` file in lexical order, then builds. Reproducibility binding:
**`10d45f438d82039ad7e79c280a8b5859473acba9` + `patches/*.patch` = the binary that produced the
fixtures**.

> **Note:** astar working tree had uncommitted changes under `vendor/iaxclient/lib/libiax2` at snapshot time.
> Those are intentionally NOT vendored. Record them as `patches/NNNN-*.patch` if they
> are needed for the captures.
