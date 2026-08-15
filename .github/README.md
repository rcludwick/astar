# .github — deliberately almost empty

The CI gate for this repository is [`.gitlab-ci.yml`](../.gitlab-ci.yml), not
GitHub Actions. Actions are switched off across Rob's repos for billing, so
nothing here is push-triggered.

The one workflow present, [`workflows/docs-pages.yml`](workflows/docs-pages.yml),
is **dormant**: `workflow_dispatch` only. It exists because publishing to
*GitHub* Pages is only reachable through the official `actions/deploy-pages`
flow — GitLab CI can build the documentation (and does, on every push, via the
`docs-site` job) but cannot publish it to Pages. Both paths call the same
`ci/build-docs.sh`.

Nothing was carried over from the pre-merge `astar` and `iaxclient-rs`
workflows. Those files were built around two separate private repos and are now
actively wrong: the cross-repo checkouts (`CROSS_REPO_TOKEN`), the
`repository_dispatch` fan-out (`DISPATCH_TOKEN`, `iaxclient-rs-updated`), and
every crate and path name in them died with the merge. Shipping them as dead
YAML would have been worse than shipping nothing.

Operational notes — runner registration, what the image must carry, and the
public-repo/self-hosted-runner conflict — are in [`../ci/README.md`](../ci/README.md).
