---
id: D-0012
title: PR-triggered CI runs on GitHub-hosted runners only, never the self-hosted pool
status: active
guards:
  - .github/workflows/ci.yml
origin: recorded
verified: true
---

## Decision

Every job in `ci.yml` pins `runs-on` to a GitHub-hosted image. The
`check-runners` job and the `dodi-smart/runner-fallback-action` self-hosted
fallback were removed from this workflow. `build-release.yml` keeps both.

## Why

`ci.yml` triggers on `pull_request` and its `rust` job compiles and runs the
contributor's code — `cargo test`, `build.rs`, proc-macro expansion. On a public
repo that is arbitrary code execution by anyone who gets a PR approved. The
`self-hosted,build` pool is persistent and sits inside our network, so one
poisoned PR buys lateral access plus a foothold that survives into later
legitimate runs. GitHub's "require approval for first-time contributors" does
not help against a repeat contributor. Ephemeral hosted runners cap the blast
radius at a throwaway VM.

`build-release.yml` is safe on the same pool because it triggers only on
`workflow_run` and `workflow_dispatch`, neither reachable from a fork PR.

## Rejected

- **Keep the fallback, rely on approval gates** — approval is per-contributor,
  not per-diff; it stops drive-by PRs, not a patient one.
- **Split fork vs same-repo PRs across runner pools** — doubles the job matrix
  to protect a runner-minutes optimization that public repos make free anyway.

## Agent directives

- Never add `self-hosted` (or a runner-fallback action) to a workflow that
  triggers on `pull_request`, `pull_request_target`, or `issue_comment`.
- Adding a self-hosted lane to `build-release.yml` is fine; adding a
  `pull_request` trigger to `build-release.yml` is not.
