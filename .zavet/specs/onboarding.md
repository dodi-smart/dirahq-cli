---
title: Onboarding — dira onboard and the installer handoff
version: 2
origin: session
verified: false
confidence: high
date: 2026-09-09
paths:
  - cli/dira/src/onboard/**
  - cli/dira/src/which.rs
  - cli/dira/src/cloud_init.rs
  - install.sh
  - install.ps1
  - docs/getting-started.md
decisions: [D-0001, D-0004, D-0007, D-0009, D-0021, DIRASH-0022, DIRASH-0024, DIRASH-0029, DIRASH-0030, DIRASH-0038]
checks:
  - cloud:repo step decision logic :: cargo test -p dira --bin dira -- onboard::steps::tests::cloud_repo
  - end-to-end: wires the repo and a second run is a no-op :: cargo test -p dira --test onboard_e2e yes_wires_the_repo_for_cloud_agents_and_a_second_run_is_a_noop
  - end-to-end: an older pin is refreshed without touching current files :: cargo test -p dira --test onboard_e2e an_older_pin_is_refreshed_without_touching_current_files
  - end-to-end: a newer pin is left alone :: cargo test -p dira --test onboard_e2e a_newer_pin_is_left_alone
  - end-to-end: --no-cloud skips the repo wiring :: cargo test -p dira --test onboard_e2e no_cloud_skips_the_repo_wiring
---

## Overview

`dira onboard` takes a machine from "the binaries are on PATH" to "capture is
running, supervised, and reporting". It exists because the product had no
single entry point and four disagreeing descriptions of the path. The two
descriptions most users met, the installers' Next-steps, omitted `dira init`
and so produced a daemon that captured nothing.

The command is a guided pass over commands that all still exist independently.
`dira init`, `dira daemon install`, `dira device link` and `dira zavet install`
are unchanged; onboarding calls them in the one order that works and reports
what happened.

## Behavior

Detection (step 1, in `detect::run`) plus seven steps that each return a
`StepOutcome`: `Done`, `AlreadyDone`, `Skipped(reason)` or `Failed(err)`.
None of them abort the run, in the dependency order `mod::run` calls them:

| Step | Does | Skips when |
|---|---|---|
| harnesses | Wires every confirmed harness at **global** scope, one pass | none detected at global scope, or all wired |
| daemon | Stops a bare daemon, then `daemon install` | `--no-service`, already supervised, declined |
| device | Prompts for a link code; blank skips. A successful link updates `State` in place, so every later step in the same run sees it linked | already linked, blank input |
| zavet | Installs the zavet Claude Code plugin | `--no-zavet`, `claude` not on `PATH`, already installed |
| zavet:repo | Scaffolds `.zavet/` in the current repo via the plugin's own `bin/zavet` | `--no-zavet`, not a git repo, `.zavet/` already present, Windows |
| cloud:repo | Wires the current repo for cloud agents exactly like `dira cloud init` (`.dira/hook.sh`, `.dira/bootstrap.sh` pinned to the running dira's version with release digests, `.dira/.gitattributes`, portable hook entries in the project `.claude/settings.json` and `.cursor/hooks.json`, both harnesses by default since the repo is shared) | `--no-cloud`, not a git repo, declined, `--harness` names no cloud-capable harness |
| knowledge | Asks (or applies `--knowledge`) the content-sync tier; last, so the value is on disk before the daemon's next start | tier already matches |

`cloud:repo` sits between `zavet:repo` and `knowledge` on purpose: both
`zavet:repo` and `cloud:repo` are repo-scoped (they act on the current
working directory's repo, unlike every other step, which acts on the
machine), so they stay adjacent rather than interleaved with machine-scoped
steps; knowledge stays last regardless (see "Knowledge consent" below), since
its value must be on disk before the daemon's next start. After all seven,
`run()` prints a closing summary (one line per `StepOutcome`) and an
open-items block. **Neither is a step**:
neither returns a `StepOutcome` nor appears in the `results` vec the summary
renders from. They are `print_summary`/`print_open_items` in `mod.rs`, not
entries the wizard iterates.

### Detection

Two independent signals per harness: the CLI on `PATH` (`which::on_path`) and
the config directory under `$HOME`. **Either is sufficient.** A harness can be
installed but never run, or run from a GUI installer that ships no CLI.
Cursor is the case that motivated it, since the app writes `~/.cursor` and
ships no `cursor-agent` by default. Requiring both would silently skip real
installs; requiring either costs a deselect.

"Already wired" is read from `doctor::checks::read_harness_wiring`, **filtered
to global scope** (`detect::globally_wired`), so it means what `dira doctor`
means by it minus project-scope entries. A bare `dira init` run inside one
repo must not read as "this harness is done" for a command that wires the
whole machine (DIRASH-0029's "why user scope"). A partially wired harness (new
events added by an upgrade) still counts as wirable, since re-running `init`
is what closes that gap.

When `$HOME` cannot be resolved, detection reports zero harnesses rather than
falling back to probing the current directory. A repo whose own tree happens
to carry a `.claude/` would otherwise read as "Claude Code detected".

**Detection performs no writes**, including against a database that already
exists. `device_linked` short-circuits on the file's absence. For a present
file, it opens it with `Store::open_readonly` (`read_only(true)` +
`immutable(true)`; a bare `read_only(true)` alone still creates `-wal`/`-shm`
sidecars on a WAL-mode database), never `Store::open`, which would migrate and
write. Plugin presence uses `zavet_install::plugin_root_offline()`, which
reads `installed_plugins.json` rather than spawning `claude` (which
bootstraps `~/.claude.json`). See DIRASH-0029.

`cloud:repo`'s detection is `cloud_init::status`, and it holds to the same
rule by construction rather than by a second, hand-maintained probe: it
compares each generated artifact against the running binary's own template,
reads the bootstrap's embedded pin as a semver relation against the running
version, and runs the writer's own hook-injection logic against a **scratch
copy** of each project config file to count what it would change, never the
real file. Nothing here fetches digests (that only happens in `apply`) or
touches the filesystem outside the scratch copy, so the read is both
write-free and network-free, and the reader cannot silently drift from what
the writer would actually do.

### Modes

- Interactive (a terminal on both stdin and stdout): prompts, defaults shown.
- `--yes`: `Auto` UI, every prompt takes its own default, narrated so the user
  sees what was decided. Resolves `--knowledge` to `full` up front.
- `--print`: renders the plan and returns before any step. Side-effect free.
- Neither a terminal nor `--yes`: prints the plan, says how to proceed, exits
  0. Never hangs, never acts.

### The daemon ordering

`dira daemon install` cannot bind the control socket while a bare-started
daemon holds it. D-0009 makes that socket the single-instance guard. The step
therefore stops a running unsupervised daemon first. This is not a
convenience: the previously documented path (`daemon start`, then `daemon
install`) fails at exactly this point.

If the service manager refuses, the step falls back to a plain start and says
the daemon will not survive a reboot, rather than reporting success.

### zavet

Scaffolding shells out to the plugin's own `bin/zavet` (`init`, then
`adapters`), resolved from the plugin root. Never from a repo's vendored
`.zavet/bin/zavet`, which is the artifact being regenerated. `zavet hooks
install` is never run and `core.hooksPath` is never written (DIRASH-0024); the
hook files are written and the user is told the one command that activates
them.

The scaffold is structurally complete and semantically empty:
`RULES.md` carries a placeholder. The summary says so, pointing at
`/zavet:init`.

### Knowledge consent

Its own step, its own prompt, naming the content `full` sends. The disclosure
prints on **every** path: interactive, `--yes`, and an explicit
`--knowledge <tier>`. It is not only the one that stops to ask; a
non-interactive run must not act on content consent silently. Defaults to
`full`, declines to `metadata`, never bundled into linking or billing
consent. Reported as pending when the device is unlinked, since the
daemon's flush is gated on the link. Because `device`'s successful link
updates `State` in the same run, a device linked earlier in that run is
correctly reported as linked here rather than stale-pending. See
DIRASH-0030.

## Interfaces & data

- `onboard::Options`: the parsed invocation, independent of clap.
- `onboard::StepOutcome`: the four outcomes, rendered with `doctor`'s
  four-shape marker convention so piped output stays distinguishable.
- `onboard::prompt::Ui`: `confirm` / `line` / `say`. Three impls:
  `Interactive`, `Auto` (flags), `ScriptedUi` (tests, records what was asked).
- `onboard::detect::State`: everything step 1 learned; steps never re-probe.
- `init::wire`: the single per-harness dispatch, shared with `dira init`;
  `init::WIRABLE`/`is_wirable`: what it accepts (deliberately narrower than
  `canonical_harness_id`, which also resolves `generic`).
- `init::Wired`: what one harness's wiring did, with `Kind` deciding the
  report wording; `init::OnUnparseable`: the corrupt-config policy
  (`Overwrite` for `dira init`, `Refuse` for onboard).
- `which::on_path`: shared with `zavet_install`'s `claude` probe.
- `config_cmd::set_quiet` / `set_quiet_at`: `set` without the report, same
  validation; `set_quiet` resolves the real XDG path and delegates to
  `set_quiet_at`, which holds the whole write body. `steps::knowledge` takes
  the write as an injected `&dyn Fn(&str) -> Result<PathBuf>` rather than
  calling `set_quiet` directly, so an in-process test can substitute a
  recording stub instead of touching the developer's real `config.toml`.
- `Store::open_readonly`: read-only, immutable, never migrates; what
  detection's device-link probe opens an existing db with.
- `cloud_init::status` / `cloud_init::apply`: the read/write pair behind
  `cloud:repo` and `dira cloud refresh` alike. `status` is the write- and
  network-free detector described above; `apply` is the one function that
  actually fetches digests and writes the artifacts, called from `cloud:repo`,
  `dira cloud init` and `dira cloud refresh` — never from detection.
- `onboard::steps::CloudApplier`: the seam `cloud:repo` calls `cloud_init`
  through, so the step's decision logic (what to run, what to tell the user)
  can be unit-tested against a fake without touching the filesystem.
  `SystemApplier` is the real implementation, wired in production.

## Invariants

- No step aborts the run; `Failed` is recorded, never propagated.
- Detection never writes and never spawns a process that writes.
- Re-running yields `AlreadyDone` for finished steps and changes nothing.
- `--print` leaves the filesystem byte-identical.
- Without a terminal and without `--yes`, nothing is changed.
- Onboarding never sets `core.hooksPath`.
- A harness accepted by `--harness` is one `init::wire` can actually wire, so
  a bad id fails before any step runs rather than mid-run.
- "Already wired" is judged at global scope only; a project-scope wiring never
  counts as done for onboarding.
- The knowledge tier is restated in the summary on every path, including the
  disclosure of what `full` sends.
- A device linked earlier in a run is treated as linked by every step that
  runs later in that same run.
- Detection reads the repo's cloud status without network or writes.
- The bootstrap pin only moves forward.
- A re-run writes only the delta and says what it was.

## Open Questions

- The `install.ps1` prompt is unverified on Windows PowerShell 5.1; pwsh 7
  cannot reproduce its console behaviour.
- Nothing shows the knowledge tier in `dira status` or `dira doctor`, so a
  user who changes it later has no ambient reminder of what it is.
- Harness detection has no signal for non-standard config directories.
