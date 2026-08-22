# Unpublished site pages

Pages written for the documentation site that are **deliberately not published
yet**. They are ordinary site Markdown — same style, same relative links — held
outside `docs/site/` because `docs_dir = "docs/site"` in `zensical.toml`, so
anything here is invisible to `zensical build` and to search.

This is the same mechanism that keeps `docs/BACKLOG.md`, `docs/design/` and the
rest of the internal material off the site; the difference is only intent.
These are finished-ish pages waiting on their subject, not engineering notes.

## What is here and why

| Path | Held back because |
|---|---|
| `server/` | astar-server is not ready to be presented to users. `docs/site/build/server.md` still documents *building* the daemon; what is held back is what it does, every configuration knob, and the HTTP + SSE control API. |

## Publishing one again

1. `git mv docs/unpublished/<dir> docs/site/<dir>`
2. Add its pages back to the `nav` array in `zensical.toml` — every path in
   `nav` must exist or `--strict` fails.
3. Re-point the links that were softened when it came out. For `server/` those
   were in `docs/site/build/server.md` (intro and Next steps) and
   `docs/site/about/safety.md` (the `POST /key` reference), plus the
   astar-server card on `docs/site/index.md`.
4. `just docs-build` — `--strict` is what proves the link graph is whole again.
