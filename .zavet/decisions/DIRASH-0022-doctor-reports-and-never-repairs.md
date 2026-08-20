---
id: DIRASH-0022
title: dira doctor reports and never repairs, and absent evidence is a skip
status: active
guards:
  - cli/dira/src/doctor/**
checks:
  - denied never advises a bare daemon start :: cargo test -p dira --bin dira denied_is_a_failure_and_never_advises_daemon_start
  - a skip never raises the exit code :: cargo test -p dira --bin dira skip_never_raises_the_exit_code
  - the registry cannot drift from CHECK_IDS :: cargo test -p dira --bin dira the_registry_emits_exactly_check_ids_in_order
origin: session
verified: false
---

## Decision

`dira doctor` diagnoses and prescribes. It never acts. There is no `--fix`, and
no check mutates anything beyond one idempotent `meta` row proving the store
accepts writes.

Three rules follow from that and are not negotiable:

1. **Absent evidence is `Level::Skip`, never `Fail`.** A check whose inputs
   could not be gathered says so and drops out of the verdict. `Level` is
   ordered `Ok < Skip < Warn < Fail` so a skip can never raise the exit code.
2. **Exit codes are a contract**: `0` all clear, `1` at least one warning, `2`
   at least one failure. `doctor::run` returns `i32`, never `Result`.
3. **`--json` carries its own `schema` integer**, not
   `dira_contract::SCHEMA_VERSION`. Adding a check id, a `detail` key or a
   top-level field is additive and does not bump it; removing or renaming a
   field, or changing what a level means for an existing id, does.

## Why

The command exists because a machine ran for days with a dead capture channel
while every individual signal reported healthy. The failure mode being designed
against is therefore *a diagnosis nobody can act on*, and both rules target it.

**Why no `--fix`.** Every remedy doctor prints is either destructive
(`daemon restart`, `device rotate-key`, `dira init` overwriting a config) or a
one-liner the user should run knowingly. And in the incident that produced this
command, the plausible automatic fix was `dira daemon start`. That is
precisely the action that makes an elevated-daemon situation worse, and
`daemon::start` already refuses it for that reason (D-0016). A doctor that
guesses wrong while acting is worse than no doctor.

**Why skip rather than fail.** With the daemon down, five checks lose their
input at once. Failing all five produces a wall of red in which the one line
that matters, `daemon.reachable`, is indistinguishable from its own
consequences. That is the same "everything looks equally broken" experience the
command was written to replace.

**Why `run` returns `i32`.** An `Err` out of `main` prints to stderr and exits
1, which is doctor's *warning* code. A gather that panicked into an `Err` would
therefore be indistinguishable from "works, could be better" to an install
script. Making every gather failure a `Fail` check at the point it happens is
the only way to keep the exit code honest, and the signature is what enforces
it rather than hoping reviewers notice.

**Why a separate `schema`.** `/contract` is drift-gated and vendored by the
cloud repo; doctor's stdout is neither. Coupling them would mean a CLI output
tweak bumping the attestation wire version.

## Rejected

- **`--fix` for the "safe" subset.** There is no safe subset: the two most
  common remedies restart a daemon and rewrite a harness config. Naming a
  subset safe is how it stops being safe.
- **Omitting ungatherable checks entirely** instead of emitting a `skip`. It
  makes `--json` consumers handle a variable key set, and it hides the
  difference between "not checked" and "checked, fine".
- **Reusing `SCHEMA_VERSION` for `--json`.** Ties a local CLI surface to the
  cross-language wire contract for no benefit.
- **`ExitCode`/`Termination` instead of `process::exit`.** Would rework `main`'s
  return type and five existing exit sites to change nothing observable.

## Agent directives

- Never add an action, repair, or auto-remediation to `dira doctor`. A new
  check reports; if it wants to fix something, that is a different command.
- A new check whose inputs are unavailable MUST return `Check::skip`. Reserve
  `fail` for evidence that something is actually broken.
- Never let `doctor::run` (or anything it calls) return `Result` to `main`.
- Every new check id goes in `CHECK_IDS` in registry order. It is both the
  `--check` allow-list and the documented `--json` key set, and a test pins the
  runner against it.
- `daemon.reachable`'s `Denied` arm must never advise a bare
  `dira daemon start`. This arm is unreachable from CI on every platform we
  build on; the unit test is the only guard it has.
- In `--json` mode, stdout carries exactly one object. Nothing else may print
  there, including `hook_health::maybe_warn` and the update notice.
- Bump the `--json` `schema` only for a removal, a rename, or a changed level
  meaning. Additions are free.

## Verification

Unit tests in `cli/dira/src/doctor/`, none needing a daemon: the `Denied`
regression guard (fails, and its remedy is neither bare `dira daemon start` nor
"not running"); `skip_never_raises_the_exit_code` plus the full exit-code
matrix; `daemon_dependent_checks_skip_when_the_daemon_is_down` over the real
runner; and `the_registry_emits_exactly_check_ids_in_order`.

Verified by hand on macOS against a live daemon (exit 0/1), with the daemon
absent (exit 2, four skips, store checks still reporting), and for
`--check`/unknown-id/piped-output behaviour.

**Not verified on Windows**, which is where the motivating incident happened.
The `Denied` path and the elevation advice are covered by pure judge tests
only. `elevation::is_elevated()` gives any CI runner exactly one token.
