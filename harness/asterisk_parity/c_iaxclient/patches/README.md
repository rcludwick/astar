# Patch series

Patches applied in lexical order on top of the `git archive HEAD` snapshot
at `../vendored/libiax2/` before the Dockerfile builds.

| Order | File | Purpose |
|-------|------|---------|
| 0001  | `0001-dcallno-0-on-resent-NEW.patch` | Reset peercallno before resending NEW post-CALLTOKEN so the hub keys a fresh session; needed against live ASL3. |

## Apply convention

All patches in this directory are written relative to astar's path layout
(e.g. `vendor/iaxclient/lib/libiax2/src/iax.c`). The Dockerfile applies
them with `git apply -p5 --directory=/src/libiax2`, stripping the
`a/vendor/iaxclient/lib/libiax2/` prefix (5 components including the
leading `a/`) so paths resolve to `/src/libiax2/src/...`.

## When to add or refresh

If the user's working-tree changes in astar diverge meaningfully from this
series, snapshot the new delta as the next `NNNN-*.patch` and update the
table above. The base snapshot is rebuilt by `../snapshot.sh` whenever
astar HEAD moves.

If a patch lands upstream in astar, drop the file here and bump the
SNAPSHOT.md "Astar repo HEAD" SHA by re-running `snapshot.sh`.
