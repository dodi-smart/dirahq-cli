---
title: Diagnostics — dira doctor and the capture probe
version: 1
origin: session
verified: false
confidence: high
date: 2026-08-09
paths:
  - cli/dira/src/doctor/**
  - cli/dirad/src/probe.rs
  - cli/dira/src/hook_health.rs
  - cli/dira/src/init.rs
decisions: [D-0008, D-0009, D-0016, D-0019, D-0020, D-0021, DIRASH-0022, DIRASH-0023]
---

## Overview

`dira doctor` answers one question the product previously could not answer at
all: **is capture actually working?**

Every signal it reports already existed somewhere. None of them composed, and
each printed from its own module and returned nothing. That is not a cosmetic
problem. A machine ran for days with a completely dead capture channel — 303
events, every one from a manual timer, zero agent events — while
`dira daemon status` reported a healthy daemon, the loopback ingress was
listening, commit capture worked, and cloud sync was green. The one broken link
was the hook shim's connect to the control channel, and nothing exercised it.

The command has two halves. Thirteen **static checks** read the daemon, the
store, the harness configs and the cloud state. One opt-in **capture probe**
(`--probe`) injects a synthetic hook through the command string the harness
config actually invokes and verifies a row lands — the only check that would
have caught the incident above.

## Behavior

- Checks run in cause-first order: daemon reachability, then the daemon's own
  self-report, then the store, then hook wiring, then the cloud. A user reading
  top to bottom meets the root cause before its consequences.
- Facts are gathered once (one `DaemonInfo` round-trip, one supervision probe,
  one `Store::open`), then judged by pure `fn`s. The checks are not independent
  — version skew needs the same round-trip reachability made — so a registry of
  independent async closures would force either N round-trips or shared mutable
  state.
- A check whose inputs could not be gathered reports `skip`, never `fail`. With
  the daemon down the report is one red line and four greyed-out skips, not five
  failures competing for attention.
- Exit codes: `0` all clear, `1` any warning, `2` any failure. `--check <id>`
  is validated against `CHECK_IDS` before any IO; an unknown id is a usage error
  (exit 2), not a diagnosis.
- `--json` emits exactly one object on stdout and nothing else. `hook_health`'s
  stderr warning and the update notice are both suppressed there.
- Human output uses four distinct glyphs (`●▲✕·`), not four colours, because
  `theme::stdout_color()` is false for pipes and a redirected report must stay
  readable — that is the form people paste into an issue.
- `daemon.reachable` distinguishes `Denied` from `Down`. A daemon that is
  running and refusing us must never be answered with a bare
  `dira daemon start`; the remedy is `elevation::access_denied_advice`.
- `hooks.config` asks whether *a* dira hook entry exists, not whether it names
  *this* binary — matched on the command's suffix, because the pre-upgrade
  unquoted form can contain spaces. Binary identity is `hooks.exe_path`'s
  question, and it expands `$HOME`/`~` before testing existence, reporting
  anything else as unverifiable rather than as a false alarm.
- `--probe` runs last and only on request. It spawns a child and writes+deletes
  a row, so keeping the default side-effect-free is what makes a bare
  `dira doctor` safe for `install.sh` and CI.
- The probe reports the furthest stage reached: spawn failed / child ran but
  could not deliver / delivered and acked but nothing landed. Those three have
  entirely different remedies.

## Interfaces & data

```rust
// cli/dira/src/doctor/mod.rs
pub(crate) enum Level { Ok, Skip, Warn, Fail }   // Ord: Skip < Warn, so a skip
                                                 // can never raise the exit code
pub(crate) struct Check { id, level, summary, remedy, detail }
pub(crate) const CHECK_IDS: &[&str];             // registry order + --check allow-list
pub(crate) async fn run(config: &Config, args: Args) -> i32;   // never Result
pub(crate) fn exit_code(checks: &[Check]) -> i32;

// cli/dira/src/doctor/capture.rs
pub(crate) enum Stage { NotConfigured, Unparseable, DaemonTooOld, ArmRefused,
                        SpawnFailed, ChildFailed, NoRowLanded, Landed }
pub(crate) fn to_check(stage: &Stage, ctx: &Ctx) -> Check;      // pure
pub(crate) fn split_command(cmd: &str) -> Option<Vec<String>>;  // never a shell

// cli/core/src/model.rs
pub const PROBE_SESSION_PREFIX: &str = "dira-probe-";
pub fn is_probe_session(session_id: &str) -> bool;
pub fn probe_hook_payload(session_id: &str, cwd: &str) -> serde_json::Value;

// cli/core/src/protocol.rs — local control protocol, NOT the /contract wire schema
Request::CaptureProbe { phase: ProbePhase }      // ProbePhase::{Arm, Verify}
Response::CaptureProbe(Box<CaptureProbeView>)

// cli/dirad/src/probe.rs
pub const ARM_TTL: Duration = Duration::from_secs(30);
pub async fn arm(state) -> Response;
pub async fn verify(state, session_id, wait_ms) -> Response;
pub fn admit(state, session_id) -> Result<(), String>;   // the ingress guard
pub fn note_landed(state, session_id);                   // called by the writer
```

`doctor::run` returns `i32`, never `Result`. An `Err` out of `main` exits 1 —
which is doctor's "warnings" code — so a broken probe could otherwise
masquerade as a warning to an install script. The signature is what enforces it.

`--json` carries its own `schema` integer (currently `1`), deliberately not
`dira_contract::SCHEMA_VERSION`: doctor is a local CLI surface, not the
drift-gated attestation wire. Adding a check id, a `detail` key, or a top-level
field is additive and does not bump it; removing or renaming a field, or
changing what a level means for an existing id, does.

## Invariants

- **Absent evidence is `skip`, never `fail`.**
- **doctor reports, it never repairs.** There is no `--fix`.
- **`daemon.reachable`'s `Denied` arm never advises a bare `dira daemon start`.**
  Pinned by a unit test; unreachable from CI on every platform we build on.
- **The daemon mints the probe session id, never the CLI**, and admits it only
  while its own arm is live and unexpired.
- **The daemon never spawns the probe child.** It may be the elevated process,
  and a forked child would inherit that token — the probe would pass on exactly
  the machine the bug is on.
- **`verify` reaps unconditionally**, including on the failure path.
- **Every `Store` read filters the reserved prefix**, except `max_event_id`,
  which is only an upper bound and is documented as deliberately unfiltered.
- **`SessionRegistry::observe` refuses probe rows**, because `partial_rollups`
  ships from the live registry and no SQL filter reaches it.
- **`DIRA_HOOK_PROBE` never writes `hook_health`.** A diagnostic must not
  overwrite the evidence it exists to report on.
- **The harness hook contract is otherwise unchanged**: a hook still always
  exits 0 and never writes stdout. Probe mode is the sole exception, and only a
  process `dira doctor` spawned itself can be in it.

## Open Questions

- The `Reach::Denied` and elevated-daemon paths cannot be exercised by CI on any
  platform we build on — `elevation::is_elevated()` gives a runner exactly one
  token. Both are covered by pure judge tests only, following `elevation.rs`'s
  own testability rule, and neither has been verified on a real Windows host.
- Only Claude Code is probed. The other five harnesses are covered by
  `hooks.config`/`hooks.exe_path` but not end to end; Codex is absent entirely
  because `dira init codex` only prints a snippet, so there is no file we own.
- `install.sh` does not yet key off the 0/1/2 exit codes. Once it does, the
  scheme becomes an external contract and changing a check's level becomes a
  breaking change.
- `daemon.supervision` reports the launchd/systemd agent even when the probed
  socket is a different (e.g. test-isolated) one. Harmless in practice, but the
  two facts are gathered independently and could disagree.
