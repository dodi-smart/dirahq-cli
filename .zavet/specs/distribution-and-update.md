---
title: Distribution and self-update
version: 2
origin: session
verified: false
confidence: high
date: 2026-07-22
paths:
  - install.sh
  - install.ps1
  - cli/dira/src/update/**
  - cli/dira/src/daemon.rs
  - cli/ipc/**
  - .github/workflows/build-release.yml
decisions: [D-0002, D-0003, D-0004, D-0005, D-0006, D-0007, D-0008, D-0009, D-0010, D-0011]
---

## Overview

How a stranger gets `dira` onto a machine and how it keeps itself current:
a `curl | sh` installer (macOS/Linux/WSL) and an `irm | iex` PowerShell
installer (native Windows), both served from the landing site, GitHub
Releases as the only artifact source, and an in-binary updater that swaps
both executables and restarts whatever is supervising the daemon.

## Behavior

- `install.sh` detects OS/arch, resolves a version, downloads the archive and
  its checksum, verifies sha256, then installs both binaries atomically. It
  never edits a dotfile, never installs a service unless asked, and makes no
  network call beyond the artifact host. On WSL it prints a note pointing
  native-Windows users at `install.ps1`; on a native-Windows shell it errors
  toward `install.ps1` (D-0010: no Windows branch in the POSIX script, ever).
- `install.ps1` mirrors install.sh section-for-section: PowerShell 5.1 floor,
  truncation-safe all-functions layout, mandatory `Get-FileHash` verify,
  same-dir staged swap that renames a locked running exe aside first
  (D-0003), dev-install refusal (D-0004), user-scope PATH via
  `[Environment]::SetEnvironmentVariable` (never `setx`), default bin dir
  `%USERPROFILE%\.local\bin`.
- Target selection is deliberately coarse: macOS maps to one universal
  binary regardless of arch, Linux to `${arch}-unknown-linux-musl`, Windows
  to `${arch}-pc-windows-msvc` zips. There is no arch branch on macOS and no
  libc probe on Linux (D-0002/D-0010).
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
- Every one of those probes dials the control socket, so they all depend on
  client and daemon resolving the same path with no coordination — hence the
  fixed per-user socket location (D-0008). `dira daemon restart` (and the
  restart inside `dira update`) detects a bare pre-D-0008 daemon still bound
  to the old `$TMPDIR` socket, kills it, removes its socket and pidfile, and
  starts a fresh daemon on the fixed path, rather than treating it as
  `NotRunning` and no-op'ing. If its pid can't be determined, restart errors
  with manual instructions instead of starting a second daemon beside it.
- `dira daemon status` exits 0 when any daemon is running — healthy,
  degraded, or still on the legacy socket — and 1 when none is; `install.sh`
  keys its post-upgrade restart decision off that exit code, so a legacy
  daemon must count as "running" (a restart will migrate it) rather than
  "down" (which would make install.sh skip the restart and strand it).

## Interfaces & data

- `install.sh` is configured entirely by `DIRA_*` environment variables and
  flags of the same name; flags win. `DIRA_BIN_DIR` is shared with the
  `justfile` on purpose so the dev and released paths agree on one location.
- Release artifacts are `dira-<version>-<target>.tar.gz` (unix) or `.zip`
  (windows) with a **flat** root (`dira`/`dirad`, or `dira.exe`/`dirad.exe`,
  at top level) plus `dira-<version>-<target>.sha256` — an extension-less
  base name, containing one `sha256sum`-style line per asset produced in
  that job. Consumers must select the line whose filename field matches the
  archive, not line 1.
- `cli/dira/src/update/` splits into `resolve` (API, channels, semver),
  `artifact` (target detection, download, checksum, extract), `replace`
  (install discovery, atomic swap, rollback), and `notice` (the passive
  update-available line).
- The archive is hashed by streaming it through a 64 KiB buffer into the
  sha2 hasher: sha2 0.11 (digest 0.11) dropped the hasher's `io::Write` impl,
  so `io::copy` is no longer available. Constant-memory either way.
- TLS for the download (and every other cloud call) validates against roots
  compiled into the binary, not the host trust store — D-0011. That is what
  keeps the static musl artifact self-sufficient on an unknown host.
- Extraction shells out to `tar -xzf` on unix rather than vendoring
  `tar`+`flate2`; `tar` is already a hard requirement of the installer. On
  windows the updater uses the `zip` crate (target-gated dep) — no
  guaranteed tar on constrained images, and the archive contents are
  restricted to the two expected binary names (zip-slip guard).
- Daemon supervision spans launchd, systemd-user, a windows logon scheduled
  task (`schtasks`, HKCU Run-key fallback), and pidfile. Windows stop is
  graceful `Request::Shutdown` over the control pipe, escalating to
  `taskkill /F`. Known gap: a logon task does not resupervise a crash
  (RestartOnFailure task XML is the follow-up).

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
- The control socket is never resolved through `$TMPDIR` (D-0008): it differs
  per process, so a client and a healthy daemon would silently miss each
  other and the daemon would present as "down".

## Open Questions

- `--bin-dir` does not isolate the daemon restart: supervision detection
  probes the fixed global `sh.dirahq.dirad` label, so updating a secondary
  install still restarts the machine's daemon. Should `--bin-dir` imply
  `--no-restart` unless it matches the directory the running daemon was
  launched from?
- The gnu Linux targets are dropped rather than deprecated over a release or
  two. If a musl-specific problem surfaces (DNS resolution differences, the
  allocator under sustained load) there is no published fallback artifact.
- Windows arm64 ships untested (GitHub's `windows-11-arm` runner label fails
  on private repos — TODO(public) in build-release.yml), the exes are
  unsigned (Authenticode is a pre-public-launch follow-up; unsigned
  logon-task daemons are prime AV false-positive material), and the schtasks
  ONLOGON + HKCU-fallback path needs validation on a real Windows machine.
