---
id: D-0010
title: Native Windows targets are added to the release matrix and install.ps1 ships alongside install.sh
status: superseded
superseded-by: D-0014
supersedes: D-0002
guards:
  - .github/workflows/build-release.yml
  - install.sh
  - install.ps1
origin: recorded
verified: true
---

## Decision

The release matrix gains `x86_64-pc-windows-msvc` (native on `windows-latest`)
and `aarch64-pc-windows-msvc` (cross-compiled on `windows-latest` via
`rustup target add`, since MSVC cross-links a foreign arch natively;
`windows-11-arm` fails in private repos). Windows legs package as `.zip`
(`zip: windows`), unlike the `.tar.gz` unix legs. `install.sh`'s
`detect_target` stays branch-for-branch unchanged. It still has no Windows
case and still errors on `MINGW*|MSYS*|CYGWIN*|Windows_NT`, only with better
text pointing at the new `install.ps1` first, WSL2 second.

## Why

D-0002 rejected Windows because nothing built it yet. That has changed: a
sibling package is landing native Windows target support in the Rust
workspace, and this repo now ships a `dira.exe`/`dirad.exe` pair for it.
D-0002's actual reasoning (glibc floors motivating musl, one universal macOS
binary) is untouched. This record only lifts its "no Windows" line.

## Rejected

- **Route Windows users through `install.sh` too**. POSIX `sh` isn't the
  native shell; `install.ps1` mirrors it section-for-section instead so
  PowerShell 5.1 users get a real double-click/`irm | iex` experience, not a
  WSL2 detour.
- **Native `windows-11-arm` runner now**. Fails on private repos; deferred
  behind a `TODO(public)` until the repo is public.

## Agent directives

- Keep `install.sh`'s `detect_target` free of a Windows branch; Windows
  installs go through `install.ps1`, never through the POSIX script.
- Windows release assets are `.zip`, not `.tar.gz`. Any code assuming one
  archive extension across the whole matrix (idempotency checks, docs) must
  special-case it.
- Revisit the `windows-11-arm` cross-compile workaround (and its
  `TODO(public)`) once the repo goes public.
- D-0002's unix directives survive unchanged and carry forward here (this
  record lifts ONLY its "no Windows" line): Linux stays static musl only.
  Never add a `-gnu` target (`install.sh` has no libc probe and would
  mis-select). macOS stays one `universal-apple-darwin` binary. Never
  introduce an arch branch on Darwin in the installer or in `detect_target`.
