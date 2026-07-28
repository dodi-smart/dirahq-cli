---
id: D-0013
title: Prerelease tags build binaries automatically now that the repo is public
status: active
supersedes: D-0005
guards:
  - .github/workflows/build-release.yml
origin: recorded
verified: true
---

## Decision

`build-release.yml` no longer skips prerelease tags on automatic runs. Every
`-develop.N` tag cut by a merge to `develop` builds the full matrix and runs
the smoke legs, exactly as a stable tag does.

## Why

D-0005 skipped them to avoid paying for billed runner minutes on a private
repo, and said so explicitly: a cost decision tied to a temporary state, to be
reverted once the repo went public. It went public on 2026-07-28.

The cost is now zero — GitHub-hosted minutes are free for public repos, and
D-0015 moved every leg onto them. The benefit is real and was just demonstrated:
the first stable release was also the first time this pipeline had ever run,
and it surfaced two defects immediately (#57, #58). Exercising packaging on
every develop merge is how that stops being a release-day surprise, and it
makes `--channel prerelease` a genuine dogfooding path.

## Rejected

- **Keep the skip, dispatch manually** — that is what let an untested pipeline
  reach a GA tag.

## Agent directives

- Prerelease and stable tags now follow the same path; do not reintroduce a
  tag-shape branch in `resolve`.
- The idempotency guard (skip when the release already has assets) is now the
  only thing preventing duplicate builds — keep it.
