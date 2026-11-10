---
id: DIRASH-0051
title: The installers register the per-user service by default and hand off to dira onboard
status: active
guards:
  - install.sh
  - install.ps1
checks:
  - install.sh stays POSIX and shellcheck-clean :: sh -n install.sh
  - Codex wiring merges every event once and keeps the user's config :: cargo test -p dira --bin dira -- codex_tests
origin: session
verified: false
---

## Decision

`install.sh` and `install.ps1` register `dirad` as a **per-user** login service by default,
whether or not a terminal is attached. That is a launchd agent, a systemd `--user` unit, or the
Windows logon task / Run key (DIRASH-0048). They skip it only for:
- `--no-service` / `DIRA_NO_SERVICE`;
- `--no-daemon`;
- `--no-interactive`;
- `CI=true`;
- an elevated PowerShell (unchanged: an elevated daemon cannot serve hooks, D-0016).

There is no longer a service prompt. Every install records `update.channel` for background
updates (DIRASH-0050). With a terminal, the installer then runs `dira onboard`, so
`curl … | sh` is one command to working capture. Without one, it prints the command.

`dira init codex`, and therefore onboarding, now merges Codex's hook tables into
`~/.codex/config.toml` (or `$CODEX_HOME/config.toml`) with `toml_edit`:
- it is idempotent;
- it keeps the user's comments, other keys and other hooks;
- it refuses an unparseable file;
- `--print` still only prints the snippet.

This amends DIRASH-0029 in two places. The installer still prompts only where a terminal exists,
but it no longer asks about the service at all. And Codex is no longer a manual follow-up.

## Why

The user asked for Dira to be easy to install and onboard, and Windows users reported a daemon
that was not running after install or reboot. The prompt defaulted to "no service" whenever there
was no terminal, and that is how many people install (tooling, `irm | iex` in some hosts). The
result was a daemon that died at the next reboot and silently captured nothing.

A per-user service needs no root or admin, is fully removed by `--uninstall`, and is what makes
capture work at all. A service is a persistent change, but this one is the product working.
Asking for it is a question almost everyone must answer "yes" to, and a wrong default fails
silently.

CI and `--no-interactive` keep the old hands-off behaviour. Release smoke legs and scripted
installs must not change a machine in ways they did not ask for.

## Rejected

- **Keep the prompt, default yes on a terminal.** It still leaves every terminal-less install
  without a service. That is the reported failure.
- **Install a system-wide service.** Needs root or admin and runs outside the user's session and
  pipe ACL (D-0016).
- **Keep Codex print-only.** It left the one harness onboarding could not finish. The merge risk
  that justified it is handled by `toml_edit`, append-only edits and refusing unparseable files.

## Agent directives

- Never register the service from an elevated PowerShell.
- Keep `--no-service` and the `CI`/`--no-interactive` hands-off paths working.
- `install.sh` stays POSIX `sh` with no Windows branch (D-0002/D-0010).
- Not yet validated on a real Windows standard account or a fresh Linux box (`verified: false`).
