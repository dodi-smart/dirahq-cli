---
id: D-0015
title: The release pipeline runs entirely on GitHub-hosted runners
status: active
guards:
  - .github/workflows/build-release.yml
origin: recorded
verified: true
---

## Decision

`build-release.yml` has no `check-runners` job and no self-hosted fallback.
Every build and smoke leg names a GitHub-hosted runner directly; the
`x86_64-unknown-linux-musl` leg that used to route to `self-hosted,build` is
plain `ubuntu-latest`.

## Why

The org's Default runner group sets `allows_public_repositories: false`. Once
this repo went public, GitHub simply stopped scheduling any job requesting
those labels. It **queues forever rather than failing**, which is the worst
shape of breakage: no error, no timeout, a release that never finishes.

Observed directly: a dry-run dispatch had every other leg green while
`Build x86_64-unknown-linux-musl` sat queued for 25+ minutes with three
online, idle, correctly-labelled runners available.

`runner-fallback-action` cannot rescue this. It selects on whether runners are
online and idle, they are, and has no visibility into the group policy, so
it confidently picks a pool that will never accept the job. A fallback that
only triggers on "no runner online" is the wrong guard for "runner exists but
policy forbids it."

Raising `allows_public_repositories` would fix the queueing and is the wrong
trade: it reopens exactly the exposure D-0012 closed, on the pipeline that
handles release artifacts. Public repos get free GitHub-hosted minutes, so
there is nothing to buy back.

## Rejected

- **Set `allows_public_repositories: true`**. Trades a security boundary for
  runner minutes that are already free.
- **Keep the fallback, add a policy probe**. More moving parts guarding a
  pool we no longer have a reason to use.

## Agent directives

- Never reintroduce `self-hosted` labels into a workflow in this repo while it
  is public. They queue silently instead of failing loudly.
- If a job hangs in `queued` with runners visibly idle, check the runner
  group's `allows_public_repositories` before debugging the workflow.
