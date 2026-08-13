---
title: Distribution and self-update
version: 6
origin: session
verified: false
confidence: high
date: 2026-08-09
paths:
  - install.sh
  - install.ps1
  - cli/dira/src/update/**
  - cli/dira/src/daemon.rs
  - cli/ipc/**
  - .github/workflows/build-release.yml
decisions: [D-0003, D-0004, D-0006, D-0007, D-0008, D-0009, D-0011, D-0013, D-0014, D-0021, DIRASH-0024]
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
- Both installers make the same best-effort `dira daemon` calls around the
  swap (`status`, `stop`, `restart`, `uninstall`) and ignore their failures.
  install.sh neutralises them with `|| true`; install.ps1 routes every one
  through `Invoke-BestEffort`, which is not merely `|| true` in PowerShell
  clothing. It also drops `$ErrorActionPreference` to `Continue` for the
  duration of the call, because Windows PowerShell 5.1 converts a *redirected*
  native command's stderr into error records that are terminating under
  `Stop` — and `dira daemon uninstall` shells out to `schtasks`/`reg`, which
  print `ERROR: ...` whenever there is nothing to remove. Before that, the
  call threw on every clean machine, its exit code was never read, and the
  scheduled-task teardown always fell through to its fallback branch.
- Target selection is deliberately coarse: macOS maps to one universal
  binary regardless of arch, Linux to `${arch}-unknown-linux-musl`, Windows
  to `${arch}-pc-windows-msvc` zips. There is no arch branch on macOS and no
  libc probe on Linux (D-0002/D-0010).
- Version resolution has two paths. Unauthenticated scrapes `tag_name` with
  grep/sed and constructs asset URLs — no `jq`. Authenticated (a token in the
  environment) resolves asset ids via `jq` and fetches with
  `Accept: application/octet-stream`, because `browser_download_url` is not
  bearer-fetchable on a private repo. Now that the repo is public the
  unauthenticated path is the one real users take; a token is worth setting
  only to lift GitHub's 60 req/hr per-IP anonymous rate limit, which is the
  failure the installer's error text points at.
- `dira update` resolves → downloads → verifies → swaps → restarts, then
  asserts the installed binary reports the expected version. `--check`
  resolves only and exits 0 in every non-error case, including offline, so it
  is safe in a script.
- Every network step is bounded and retried. The artifact download makes up to
  4 attempts on a 500ms-seeded ladder capped at 4s, retrying transport
  failures, timeouts, 5xx and 429 (honouring `Retry-After`, itself capped);
  a 4xx is **never** retried, because it is deterministic — the 404 keeps its
  "asset not found on that release" wording and fails on the first attempt.
  Timeouts are per-request rather than client-wide (a small JSON API response
  and a ~20MB artifact want very different budgets), plus a shared 10s
  `connect_timeout`. Both installers already retried (`install.ps1`'s 3-attempt
  loop, `install.sh`'s `curl --retry 3`); the updater was the one downloader
  that did not, so a single mid-stream abort — routine on a lossy or
  TLS-inspecting corporate link — failed the whole update. This is not
  platform-specific: it fails identically on macOS, Linux and Windows.
  *Known gap:* the body is buffered whole (`resp.bytes()`), so a retry re-pulls
  the entire archive rather than resuming with a `Range` request.
- A failed `dira update` is recorded, not just returned. The passive notice's
  cache carries a consecutive-failure count next to the resolve half, and after
  2 failures the notice escalates from "run `dira update`" to naming how many
  attempts failed. The two halves are independent on purpose: a resolve that
  succeeds says nothing about whether installing works, and that exact
  combination — a healthy check advertising a version every update attempt
  fails to install — is what let a user lose a week to a retryable blip with no
  signal anywhere. The count survives `write_sentinel`'s TTL rollover, or it
  would silently un-escalate about once a day. Writing happens only in
  `update::run`; the foreground notice path stays read-only and network-free
  per D-0006.
- Every comparison of a resolved release against the **running** version goes
  through `resolve::compare_versions` (SemVer 2.0 §11), never string equality.
  Three callers share it — `--check`'s message, the passive notice, and the
  downgrade guard — and an unorderable version means "make no claim", never
  "different, therefore newer". Being *ahead* of a channel is a normal state,
  not an upgrade: a prerelease against the stable channel reports up to date
  (`--check` names the channel head it is ahead of; the passive notice stays
  silent, since it fires on ordinary commands).
- A `GH_TOKEN`/`GITHUB_TOKEN` the API **rejects** is never fatal, in any of the
  three code paths that read it (`install.sh`, `install.ps1`, `dira update`).
  On a public repo a token is purely an optimization — it lifts GitHub's
  60 req/hr anonymous per-IP limit and nothing else — so a 401 drops the token,
  warns once on stderr, and resolution continues anonymously. An expired or
  wrong-account token exported in a user's shell is common and has nothing to
  do with dira; before this it made the product uninstallable and
  un-updatable. Dropping the token (rather than retrying the one call) is what
  also moves the *download* off the authenticated asset-id path, which would
  otherwise resend the same rejected bearer. Only 401 is recoverable — a 404 or
  a 5xx stays fatal rather than being retried as if it were an auth problem.
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

- `dira update` may refresh the zavet *plugin* (machine scope, the blast radius
  it already has) but never a repo's adapters or git hooks (DIRASH-0024). No cwd
  is resolved anywhere in that path.
- The plugin refresh runs only inside the successful swap-and-restart arm: never
  on `--no-restart`, never on `--check` (D-0006 keeps that path free of network
  I/O), and never after a rollback — a rolled-back machine must not come out of
  an update with a bumped plugin.
- An update never *installs* a plugin the user never asked for. Absent or
  inconclusive detection is a silent no-op.
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
- `dira update` never installs a version older than the running one unless
  `--version` asked for it by name. The channel-resolved path refuses with
  both escape hatches spelled out; `--force` does not override it (that flag
  means the D-0004 dev-install guard and nothing else). Without this the
  notice and `update` disagreed: the check offered stable `0.1.0` to a
  `0.1.1-develop.1` build and the command carried the downgrade out.
- The post-swap `dira --version` probe retries on `ETXTBSY`. This is the
  *other* ETXTBSY, distinct from D-0003's "never open the destination for
  writing": Linux also refuses to **exec** an inode any process holds open for
  writing, and staging plus a concurrent fork elsewhere in the process creates
  exactly that window. Rename discipline cannot prevent it; a bounded retry
  can.
- The control socket is never resolved through `$TMPDIR` (D-0008): it differs
  per process, so a client and a healthy daemon would silently miss each
  other and the daemon would present as "down".
- A successful `install.ps1` run leaves `$LASTEXITCODE` at 0 — set explicitly
  as the file's last statement, not merely "0 or untouched". The script signals
  failure by throwing, never through an exit code, so any non-zero code it
  leaves behind is spurious — and it is not cosmetic: GitHub Actions ends every
  `shell: powershell` step with `exit $LASTEXITCODE`, which is how a
  `-Uninstall` that printed every success message and removed both binaries
  still failed both Windows smoke legs of the v0.1.1-develop.1 release.
- Any caller comparing `$LASTEXITCODE` must test `Test-Path variable:` first,
  the way GitHub's own wrapper does. `$null -ne 0` is **TRUE** in PowerShell, so
  a bare `if ($LASTEXITCODE -ne 0) { throw }` fires after a script that ran no
  native command at all and therefore never created the variable. The smoke
  assertions in `build-release.yml` shipped without that guard and failed both
  windows legs of v0.1.1-develop.2 on a *successful* fresh install — the same
  null trap the #58 note in that file already warned about. Normalising inside
  `install.ps1` and guarding in the caller are both needed: one keeps the
  contract true, the other survives an older or truncated script.

## Open Questions

- `--bin-dir` does not isolate the daemon restart: supervision detection
  probes the fixed global `sh.dirahq.dirad` label, so updating a secondary
  install still restarts the machine's daemon. Should `--bin-dir` imply
  `--no-restart` unless it matches the directory the running daemon was
  launched from?
- The gnu Linux targets are dropped rather than deprecated over a release or
  two. If a musl-specific problem surfaces (DNS resolution differences, the
  allocator under sustained load) there is no published fallback artifact.
- Windows arm64 now builds and smokes natively on `windows-11-arm` (D-0014),
  closing the "ships untested" gap; the exes are still unsigned (Authenticode
  remains a follow-up — unsigned logon-task daemons are prime AV
  false-positive material), and the schtasks ONLOGON + HKCU-fallback path
  still needs validation on a real Windows machine.
- The Windows binaries link the MSVC CRT **statically** (`.cargo/config.toml`,
  `-C target-feature=+crt-static`), so they need no VC++ Redistributable on the
  target host. This closes #60 and puts Windows on the same footing as the
  other targets: static musl on Linux (D-0002), bundled TLS roots (D-0011) —
  the artifact carries what it needs rather than depending on the host.
  A build-release step asserts the binaries import neither `VCRUNTIME140` nor
  `api-ms-win-crt-*`, because nothing else can catch a regression: every GitHub
  Windows image ships the redistributable, so the smoke legs would stay green
  while shipping a binary that dies at process start on a clean box.
