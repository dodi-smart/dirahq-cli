---
title: Distribution and self-update
version: 1
origin: session
verified: false
confidence: high
date: 2026-07-20
paths:
  - install.sh
  - cli/dira/src/update/**
  - cli/dira/src/daemon.rs
  - .github/workflows/build-release.yml
decisions: [D-0002, D-0003, D-0004, D-0005, D-0006, D-0007]
---

## Overview

How a stranger gets `dira` onto a machine and how it keeps itself current:
a `curl | sh` installer served from the landing site, GitHub Releases as the
only artifact source, and an in-binary updater that swaps both executables
and restarts whatever is supervising the daemon.

## Behavior

- `install.sh` detects OS/arch, resolves a version, downloads the archive and
  its checksum, verifies sha256, then installs both binaries atomically. It
  never edits a dotfile, never installs a service unless asked, and makes no
  network call beyond the artifact host.
- Target selection is deliberately coarse: macOS maps to one universal
  binary regardless of arch, Linux to `${arch}-unknown-linux-musl`. There is
  no arch branch on macOS and no libc probe on Linux (D-0002).
- Version resolution has two paths. Unauthenticated scrapes `tag_name` with
  grep/sed and constructs asset URLs — no `jq`. Authenticated (a token in the
  environment) resolves asset ids via `jq` and fetches with
  `Accept: application/octet-stream`, because `browser_download_url` is not
  bearer-fetchable on a private repo. Only maintainers and CI take that path.
- `dira update` resolves → downloads → verifies → swaps → restarts, then
  asserts the installed binary reports the expected version. `--check`
  resolves only and exits 0 in every non-error case, including offline, so it
  is safe in a script.
- Daemon restart covers launchd, systemd-user, and pidfile supervision. The
  running daemon reports its own pid over the socket, so a missing pidfile is
  not a dead end.

## Interfaces & data

- `install.sh` is configured entirely by `DIRA_*` environment variables and
  flags of the same name; flags win. `DIRA_BIN_DIR` is shared with the
  `justfile` on purpose so the dev and released paths agree on one location.
- Release artifacts are `dira-<version>-<target>.tar.gz` with a **flat** root
  (`dira`, `dirad` at top level) plus `dira-<version>-<target>.sha256` — an
  extension-less base name, containing one `sha256sum`-style line per asset
  produced in that job. Consumers must select the line whose filename field
  matches the tarball, not line 1.
- `cli/dira/src/update/` splits into `resolve` (API, channels, semver),
  `artifact` (target detection, download, checksum, extract), `replace`
  (install discovery, atomic swap, rollback), and `notice` (the passive
  update-available line).
- Extraction shells out to `tar -xzf` rather than vendoring `tar`+`flate2`;
  `tar` is already a hard requirement of the installer.

## Invariants

- Checksum verification is mandatory and has no override flag. An
  unverifiable download is a pipeline bug, not a user decision.
- The binary being replaced is never opened for writing — only renamed onto
  (D-0003). Staging happens inside the destination directory so the rename is
  same-filesystem and therefore atomic.
- `dirad` is replaced before `dira`, so an interrupted update leaves the
  version skew that `dira version` already warns about, rather than a
  new CLI silently driving an old daemon.
- Neither the installer nor the updater will overwrite a development install
  (D-0004).
- The update check never performs network I/O on the foreground path
  (D-0006), and its notice goes to stderr so piped output is unchanged.

## Open Questions

- `--bin-dir` does not isolate the daemon restart: supervision detection
  probes the fixed global `sh.dirahq.dirad` label, so updating a secondary
  install still restarts the machine's daemon. Should `--bin-dir` imply
  `--no-restart` unless it matches the directory the running daemon was
  launched from?
- The gnu Linux targets are dropped rather than deprecated over a release or
  two. If a musl-specific problem surfaces (DNS resolution differences, the
  allocator under sustained load) there is no published fallback artifact.
