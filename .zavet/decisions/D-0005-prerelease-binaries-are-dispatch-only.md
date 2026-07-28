---
id: D-0005
title: Prerelease binaries build on manual dispatch only, while the repo is private
status: superseded
superseded-by: D-0013
guards:
  - .github/workflows/build-release.yml
origin: recorded
verified: true
---

## Decision

`build-release.yml` keeps its hard skip for prerelease tags on automatic
runs. Prerelease artifacts are produced only by an explicit
`workflow_dispatch`, which the skip already exempts.

## Why

Every merge to `develop` cuts a `-develop.N` prerelease. Building the full
matrix plus the smoke legs on each one is real billed runner time while the
repository is private, and nobody consumes those artifacts yet. Dispatch
keeps the pipeline exercisable on demand — which is how it gets proven
before the first public release — without paying for it on every merge.

This is a cost decision tied to a temporary state, not an architectural one.
Once the repo is public and runners are free, removing the skip makes every
develop merge exercise packaging and makes `--channel prerelease` a real
dogfooding path. The line carries a `TODO(public)` marker for that.

## Rejected

- **Remove the skip now** — the original plan. Reversed once the billing
  implication was weighed against the fact that nothing consumes prerelease
  artifacts yet.
- **Build prereleases but skip the smoke job** — the smoke job is the only
  thing that proves the artifacts are usable; keeping the expensive half and
  dropping the valuable half is the wrong trade.

## Agent directives

- Do not remove the prerelease skip until the repository is public. When
  removing it, delete the `TODO(public)` comment in the same change.
- `workflow_dispatch` with `dry_run: true` is the safe way to exercise the
  full matrix from any branch; it must stay wired to the packaging action's
  own dry-run input.
- The artifact facts this pipeline produces — checksum filename, flat tarball
  root, both slices in the universal binary — are hardcoded assumptions in
  `install.sh` and `update/artifact.rs`. Confirm them on a real dispatch
  before the first public release.
