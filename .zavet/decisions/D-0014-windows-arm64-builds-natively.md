---
id: D-0014
title: Windows arm64 builds on a native runner and gets its own smoke leg
status: active
supersedes: D-0010
guards:
  - .github/workflows/build-release.yml
  - install.sh
  - install.ps1
origin: recorded
verified: true
---

## Decision

`aarch64-pc-windows-msvc` builds on the native `windows-11-arm` runner instead
of cross-compiling from `windows-latest`, and gains its own smoke leg on that
runner. This lifts exactly one line of D-0010; every other directive there
carries forward unchanged.

## Why

D-0010 cross-compiled because `windows-11-arm` fails outright on private
repos, and deferred the native runner behind a `TODO(public)`. The repo went
public on 2026-07-28, so the label resolves.

The build was never the problem. MSVC cross-links a foreign arch fine, and
v0.1.0's arm64 zip built that way. The gap is testing: you cannot execute an
arm64 binary on an x64 runner, so Windows arm64 has shipped **entirely
unverified**. That is not hypothetical caution. The x64 Windows smoke leg had
also never actually run until v0.1.0, and when it did it took two rounds of
investigation to clear (#58).

## Rejected

- **Keep cross-compiling, skip the smoke**. Ships an untested binary for a
  whole architecture, which is the exact hole this closes.

## Agent directives

- D-0010's directives survive in full: no Windows branch in `install.sh`'s
  `detect_target`; Windows assets are `.zip`, not `.tar.gz`; and D-0002's unix
  rules (Linux static musl only, macOS one universal binary) carry forward.
- Verify a runner-label change with `workflow_dispatch` + `dry_run: true`
  before merging. It exercises build and packaging without uploading.
