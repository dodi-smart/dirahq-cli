---
id: DIRASH-0037
title: The portable hook yields to live user-scope wiring — no cache, no time window
status: active
guards:
  - cli/dira/src/hook_yield.rs
  - cli/dira/templates/dira-hook.sh
checks:
  - wired-and-resolvable user scope yields, missing does not :: cargo test -p dira --bin dira -- hook_yield::tests::wired_and_exists_yields hook_yield::tests::wired_and_missing_does_not_yield hook_yield::tests::wired_and_unverifiable_yields
  - a tool-scoped matcher never yields events outside it, a catch-all matcher always does :: cargo test -p dira --bin dira -- hook_yield::tests::matcher_bash_does_not_yield_other_tool_events hook_yield::tests::matcher_catch_all_forms_all_yield
  - another portable wrapper is never treated as live wiring :: cargo test -p dira --bin dira -- hook_yield::tests::another_portable_wrapper_does_not_yield
  - Cursor's flat hook shape is read the same as Claude's nested one :: cargo test -p dira --bin dira -- hook_yield::tests::cursor_flat_shape_yields
  - the marker alone never yields: both structural signals are required, either alone forwards :: cargo test -p dira --bin dira -- hook_yield::tests::marker_plus_live_user_scope_plus_project_wrapper_yields hook_yield::tests::marker_plus_live_user_scope_without_project_wrapper_forwards hook_yield::tests::project_wrapper_without_live_user_scope_forwards hook_yield::tests::neither_signal_forwards
  - a real portable invocation yields to a real live user-scope entry, a direct invocation still forwards, a stray marker with no project-scope portable wiring still forwards :: cargo test -p dira --test hook_yield_e2e -- portable_invocation_yields_to_live_user_scope_wiring direct_invocation_still_forwards_with_the_same_wiring_present stray_marker_without_project_scope_portable_wiring_still_forwards
origin: session
verified: true
---

## Decision

`.dira/hook.sh` (the repo-committed portable wrapper `dira cloud init` writes) exports
`DIRA_HOOK_VIA=portable` before `exec dira hook <harness>`. Inside `dira hook`, when that marker
is set and the invocation is not `dira doctor --probe`'s own synthetic hook, the yield is decided
by **two** structural signals, both required — the marker is a hint that gates the check, never
proof on its own that a real portable invocation is under way:

1. **User scope is live** (`hook_yield::user_scope_wires`, judged by the pure `decide`): is the
   *same event*, for the *same harness*, **also** wired at user (global) scope with a command
   that (a) is a direct `dira hook <harness>` invocation, not another portable wrapper, (b) is not
   scoped away by a real tool matcher (only an absent/`""`/`"*"`/`".*"` matcher counts as covering
   the whole event), and (c) resolves to `ExePath::Exists` or `::Unverifiable` (never `::Missing`)?
2. **Project scope genuinely wires the portable wrapper**
   (`hook_yield::project_scope_wires_portable`): does the **project**-scope config for this
   harness (`init::harness_config_paths()`'s project row, resolved against `CLAUDE_PROJECT_DIR`
   when set, else the current directory) wire the same event with a command
   `init::command_is_portable_wrapper` recognises?

`hook_yield::should_yield` is the pure AND of both. If both hold, the portable invocation exits 0
without forwarding — the live user-scope wiring will deliver this event on its own. If only the
marker and signal 1 hold — a stray `DIRA_HOOK_VIA=portable` inherited from a shell profile, on a
direct invocation in a project `dira cloud init` never wired — the invocation still forwards:
there is no real portable delivery to yield to. There is no cache and no time window: both checks
are one file read each, re-derived on every invocation.

`DIRA_HOOK_DEBUG` prints one line on a yield (`dira hook <harness>: yielded to user-scope wiring
for <event>`), mirroring the debug line already used for a *failed* forward — a yield is silent by
design (no `hook_health` write either), and this is the only way to confirm one actually happened.
This also covers a real gap `user_scope_wires` cannot see from a file read alone: a launch mode
that drops user-scope settings entirely (Claude Code's `allowManagedHooksOnly`, or any harness
config that similarly excludes them) still leaves `~/.claude/settings.json` on disk, readable and
resolvable, exactly as if it were live — so the portable invocation can yield to wiring that the
harness itself will never actually run, and the event is silently dropped end to end. There is no
structural fix for that from inside `dira hook` (the harness's own effective-config resolution
isn't observable this way); `DIRA_HOOK_DEBUG` turning "yielded" from silent into a printed line is
what makes that gap diagnosable instead of invisible.

## Why

`dira init --global` (part of ordinary onboarding) and `dira cloud init` (repo-committed, for
cloud runtimes) both wire hooks, at different scopes, for the same harness events. An engineer
who has onboarded their laptop and then opens a teleport-ready repo gets both: Claude Code runs
every scope that wires an event, so the same `Stop`/`PreToolUse`/… fires twice — once through
the portable wrapper, once through the user-scope entry — and every hook event dira counts would
double.

The fix has to be a yield, not a dedup, because a dedup needs a shared notion of "the same event"
across two independent process invocations, and neither signal available is trustworthy for that:
a daemon-side time-window dedup would drop legitimate near-simultaneous events (parallel `Read`
tool calls routinely land within the same tick), and human-originated signals (a manual
`SessionStart`/`Stop`) carry no unique id to dedup on at all. A yield sidesteps both problems: it
asks a structural question — "will someone else already deliver this?" — that is answerable
without correlating two deliveries after the fact.

The check runs unconditionally with no cache, deliberately: the two configs it reads are on local
disk, so re-reading is one `read_to_string` + one `serde_json::from_str`, indistinguishable in
cost from the fixed per-event overhead the hook loop already pays. Caching would trade that
negligible cost for a staleness window — a config edited between the daemon starting and the next
hook firing would be judged against a stale wiring picture.

## Rejected

- **Daemon-side time-window dedup** — parallel `Read` tool calls are legitimately near-
  simultaneous, so a window wide enough to catch a genuine double-delivery also catches genuine
  distinct events; and a human-originated signal has no id to key the window on at all.
- **`dira init --global` emitting the portable wrapper string instead of the direct form** — would
  make `exit 127` (no `dira` in scope) the default outcome in every repo that is not yet
  `cloud init`-wired, since the portable wrapper only resolves a binary when one already exists on
  `PATH`/the well-known install paths; the direct, absolute-path form onboarding writes today is
  what makes user-scope wiring work at all on a machine with no cloud story.
- **Requiring an exact byte match between the two commands** — rejected implicitly by design:
  the portable wrapper's command string and the user-scope entry's absolute-path command are never
  byte-identical (that is the whole reason two entries exist), so the yield judges *behavior*
  (does a live, resolvable, catch-all `dira hook <harness>` exist at user scope) rather than text
  equality.

## Agent directives

- `hook_yield::decide` and `hook_yield::should_yield` are pure — `decide` takes an injected
  `resolve: impl Fn(&str) -> ExePath`, `should_yield` takes the two already-computed booleans. Keep
  both that way; do not reach into the filesystem from inside either. Filesystem reads belong in
  `hook_yield::user_scope_wires` and `hook_yield::project_scope_wires_portable` only.
- Never collapse the two-sided check back to the marker plus `user_scope_wires` alone. The marker
  is an ordinary env var and can leak into a shell profile; without the project-scope check
  (`init::event_wired_by_portable_wrapper`, via `project_scope_wires_portable`) a direct invocation
  with that stray var set would incorrectly yield and drop the event with nothing else to deliver
  it — see `hook_yield_e2e::stray_marker_without_project_scope_portable_wiring_still_forwards`.
- The `!probe` guard in `main.rs`'s `forward_hook` (not a file this decision guards, but load-
  bearing for it) must stay structural: `dira doctor --probe` always drives the *direct* form and
  must never be capable of yielding, or the capture probe could pass on exactly the machine its own
  bug is on — see DIRASH-0023's identical reasoning for a different code path.
  Do not weaken it to a doc comment.
- Never widen "counts as live wiring" to include another portable wrapper (`init::
  command_is_portable_wrapper`'s exclusion in `command_wires_live` is load-bearing): two portable
  entries yielding to each other would silently drop the event everywhere, with no live wiring
  left to deliver it.
- Never treat a real tool matcher (e.g. `"Bash"`) as catch-all. `matcher_is_catch_all` only accepts
  absent/`""`/`"*"`/`".*"`; loosening it would make the portable path drop events a scoped
  user-scope entry never actually covers.
- If `.dira/hook.sh`'s marker constant (`DIRA_HOOK_VIA=portable`) ever changes, `hook_yield::
  via_portable` must change with it in the same commit — the two are one contract, split across a
  shell template and a Rust module for exactly the process boundary they cross.
