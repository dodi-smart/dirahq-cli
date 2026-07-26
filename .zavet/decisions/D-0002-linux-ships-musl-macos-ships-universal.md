---
id: D-0002
title: Linux ships static musl only; macOS ships one universal binary
status: superseded
superseded-by: D-0010
guards:
  - .github/workflows/build-release.yml
  - install.sh
  - cli/dira/src/update/artifact.rs
origin: recorded
verified: true
---

## Decision

The release matrix is three legs: `x86_64-unknown-linux-musl`,
`aarch64-unknown-linux-musl`, and `universal-apple-darwin`. No gnu targets,
no separate Intel and Apple Silicon macOS artifacts, no Windows.

## Why

A `-gnu` binary links against the *build* host's glibc and imposes that
version as a floor on every user. Built on ubuntu-24.04 that floor is 2.39,
which excludes Ubuntu 22.04, Debian 12, RHEL 9 and Amazon Linux 2023 — the
failure surfaces as `GLIBC_2.39 not found`, which reads to a user as "your
installer is broken". Worse, the x86_64 leg runs on a self-hosted runner
whose glibc version is unknown and can change without notice, so the floor
is not even deterministic. Static musl removes the build host as a variable
entirely and makes Alpine and containers supported rather than rejected.

The macOS side is a `lipo` fat binary that the packaging action assembles
itself. One artifact covers both architectures, which is why `install.sh`
has no arch branch on Darwin and needs no Rosetta detection — a translated
x86 shell on Apple Silicon simply works.

## Rejected

- **Keeping gnu alongside musl as an escape hatch** — doubles the Linux
  matrix to publish an artifact whose whole problem is that its
  compatibility floor is invisible until a user hits it.
- **Separate `x86_64-apple-darwin` and `aarch64-apple-darwin`** — an extra
  leg, an extra artifact, and an arch-detection branch in the installer, to
  produce something the universal binary already covers.
- **musl via the packaging action's implicit `cross` fallthrough** — it works,
  but it is inferred from that action's `main.sh` rather than documented, and
  it pulls in Docker. `taiki-e/setup-cross-toolchain-action` is explicit.

## Agent directives

- Do not add a `-gnu` or Windows target to the matrix without revisiting this
  record; `install.sh` has no libc probe and would mis-select.
- macOS is always `universal-apple-darwin`. Never introduce an arch branch on
  Darwin in the installer or in `detect_target`.
- Keep musl's two known trade-offs in mind before blaming them: the allocator
  is slower under heavy multithreaded malloc, and the resolver ignores
  `nsswitch.conf`. Neither binds a mostly-idle daemon resolving one hostname.
