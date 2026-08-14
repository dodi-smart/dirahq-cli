---
id: DIRASH-0029
title: Onboarding is one re-runnable waterfall, and the installer prompts only where a tty exists
status: active
guards:
  - cli/dira/src/onboard/**
  - install.sh
  - install.ps1
checks:
  - --print is provably side-effect free :: cargo test -p dira --test onboard_e2e print_changes_nothing_on_disk
  - a second run is a no-op :: cargo test -p dira --test onboard_e2e yes_wires_a_detected_harness_and_a_second_run_is_a_noop
  - no terminal and no --yes never acts :: cargo test -p dira --test onboard_e2e a_non_interactive_run_without_yes_prints_the_plan_and_exits_clean
origin: session
verified: false
---

## Decision

`dira onboard` is the single entry point for setting a machine up. It runs six
steps in dependency order — detect, harnesses, daemon service, device link,
zavet, verify — and three rules govern all of them:

1. **Skip, don't fail.** No step aborts the run. A step that cannot proceed
   returns `StepOutcome::Skipped`/`Failed`, the wizard continues, and the
   closing summary collects what is still open.
2. **Idempotent, with no state file.** Every step re-derives its own status
   from the machine, so a second run reports `AlreadyDone` and picks up the
   rest. There is nothing persisted that could go stale or disagree.
3. **Detection never writes.** The detection pass may not create files or
   spawn anything that does, because `--print` promises to change nothing.

`install.sh` and `install.ps1` ask — on `/dev/tty` and on an unredirected
console respectively — whether to register the daemon as a login service, and
their Next-steps collapse to `dira onboard`. With no terminal, or with
`--no-interactive`/`-NoInteractive`, the historical hands-off behaviour is
unchanged byte for byte.

Onboarding wires harnesses at **user scope**, unlike a bare `dira init`.

## Why

Four surfaces used to state four different getting-started sequences: the root
`--help`, `README.md`, `install.sh:942` and `install.ps1:1048`. The two most
users actually saw — the installers — omitted `dira init` entirely, so anyone
who followed them ended up with a running daemon that captured nothing,
silently and indefinitely. They also recommended `dira daemon start`, which
takes the control socket and thereby blocks the `dira daemon install` that was
the actually-correct step (D-0009 makes that socket the single-instance
guard). The documented path led directly into a trap.

**Why the installer may now prompt.** The old rationale — "`curl | sh` has no
usable stdin to ask with anyway", written at `install.sh:936` — was only half
true. Under `curl … | sh` stdin *is* the script being read, but `/dev/tty` is
the controlling terminal regardless of how stdin is plumbed, which is exactly
what it exists for. The half that was true is preserved: installing a
launchd/systemd agent is a persistent system change and is still never done
silently.

**Why user scope.** `dira init` defaults to project scope because you run it
inside the repo you want tracked. Onboarding sets up a *machine*; project
scope would wire only whichever directory the user happened to be standing in
and leave every other repo uncaptured with no signal why — the same
silent-undercapture failure the command exists to prevent.

**Why detection may not write.** Two probes had to be changed to hold this.
`Store::open` creates the database and runs migrations, and
`zavet_install::plugin_root()` shells out to `claude`, which bootstraps
`~/.claude.json` and a backup directory on first run. Both made `--print` a
write. The device probe now short-circuits on the database file's existence
(an absent database cannot hold a device id, so nothing is lost), and plugin
detection reads `installed_plugins.json` directly via `plugin_root_offline()`.

**Why skip-don't-fail.** The steps are independent. A harness config that will
not parse should not cost the user their daemon service or their device link,
and an early `?` would make the failure order — not the severity — decide how
much of the setup happened.

## Rejected

- **Overload bare `dira init` as the wizard.** Changes the meaning of a
  documented command, and leaves `dira init <harness>` and `dira init` doing
  fundamentally different things behind one name.
- **Install the service by default, no prompt.** Silently making a persistent
  launchd/systemd change is precisely what `docs/install.md`'s "what the
  installer does and does not touch" contract promises not to do.
- **Test stdin with `[ -t 0 ]` in install.sh.** Always false under
  `curl | sh`, which is the shape essentially every real install takes.
- **Abort the run on the first failed step.** Makes the amount of setup that
  happened a function of step ordering rather than of what actually went
  wrong.
- **Persist progress to a state file.** A second source of truth that can
  disagree with the machine. Re-deriving is cheap and cannot go stale.
- **Reimplement `zavet init` in Rust.** `bin/zavet` is ~2900 lines of POSIX sh
  and is also the *runtime* (`gate`, `index`, `emit`), so it is vendored into
  the repo regardless; a reimplementation would be a second copy that must
  agree with the first forever.

## Agent directives

- A new step returns a `StepOutcome`. It must never `?` out of the runner, and
  never `process::exit`.
- Nothing in `onboard::detect` may create a file, run a migration, or spawn a
  process that does. If a probe needs an answer only a spawn can give, add an
  offline variant and use that (see `plugin_root_offline`).
- Both `dira init` and onboarding dispatch through `init::wire`; do not add a
  second per-harness match. Onboarding passes `global: true` and
  `OnUnparseable::Refuse`, `dira init` passes `Overwrite` — the user named
  that exact file there; onboard did not.
- Validate a user-supplied harness against `init::is_wirable`, never against
  `canonical_harness_id` alone: the latter also resolves `generic`, which has
  no config to write. A new harness goes in `init::WIRABLE` **and**
  `detect::HARNESSES`; `the_three_harness_tables_agree` pins them together.
- A bare-started daemon is stopped before `daemon install`, and never reordered:
  the socket is the single-instance guard. The stop now lives *inside*
  `daemon::install` rather than in this step — see the amendment below. Do not
  re-add it here, and do not remove it from there.
- Never run `zavet hooks install` and never write `core.hooksPath`
  (DIRASH-0024 governs this; onboarding inherits it).
- In `install.sh`, ask through `_confirm <prompt> <default> <notty>`; do not
  hand-roll a second `/dev/tty` read. The `notty` argument is separate from
  `default` on purpose: a scripted `--uninstall` must proceed unattended while
  a service install must never happen unattended, so the two questions need
  opposite fallbacks.
- `install.ps1`'s Administrator branch keeps precedence over the prompt. Never
  offer to install a service from an elevated shell; that is the setup its
  warning exists to prevent.

## Verification

`cli/dira/tests/onboard_e2e.rs` drives the real binary under D-0021 discipline
(`isolate_user_dirs`, `output_staged`, `--no-service` on every invocation).
`print_changes_nothing_on_disk` compares a full recursive snapshot of the
isolated `$HOME` before and after, which is what caught both write-during-
detection bugs; the idempotency test asserts the settings file's mtime is
unchanged on a second run, not merely that the output says "already wired".

Unit tests in `cli/dira/src/onboard/` cover the decision logic behind a
scripted `Ui` — including that the knowledge disclosure names the content it
sends.

`install.sh` is verified with `sh -n` and by exercising `_can_prompt` with
stdin closed, stdin piped, and detached — all three take the silent path.
`install.ps1` parses clean under pwsh 7.4.6 (arm64 tarball; the Microsoft
container images are amd64-only and crash under qemu), and `Test-CanPrompt`
returns false in the piped/CI shape.

## Open questions

- The `install.ps1` prompt is **unverified on Windows PowerShell 5.1**, which
  is the shipped shell on a stock Windows box. pwsh 7 cannot reproduce 5.1's
  console behaviour, so `Read-Host` and the `IsInputRedirected` gate have only
  been exercised on 7.x.
- Harness detection has no signal for a harness installed under a
  non-standard config directory; such a machine falls back to `--harness`.

## Amendment: the pre-stop moved into `daemon::install` (2026-08-14)

The ordering above was right and stayed right; the *placement* was the bug.
"The daemon step stops a bare-started daemon" described three callers — this
step, `install.sh` and `install.ps1` — and silently excused the fourth. A bare
`dira daemon install` never stopped anything, so on a machine with a
hand-started `dirad` it installed a launchd `KeepAlive` / systemd
`Restart=always` / logon-task service that could not bind D-0009's socket, lost
the race, and was restarted into losing it again for as long as the old process
lived. That is the documented path for anyone who followed the old
"run `dira daemon start`" advice.

`daemon::install` now stops an unmanaged daemon itself and waits for exit, so
every caller is correct by construction. This step's own pre-stop is gone. Both
installers keep theirs, because they may be driving an older binary that lacks
this.

Two things came with it, both required rather than incidental:

- Unix `stop` now confirms the process exited (SIGTERM → wait → SIGKILL → wait)
  instead of signalling, unlinking the pidfile and socket, and printing
  "stopped". D-0019's directive is not platform-scoped; unix was exempt only
  because D-0009's `flock` makes a duplicate *safe*. Safe is not the same as
  unnecessary — a supervisor still flaps against a socket it cannot take.
- `install` is now `async`, and only stops a daemon nothing is supervising.
  Stopping a supervised one would just make its supervisor restart it mid-install.
