---
id: DIRASH-0047
title: A headless run withholds human signals at the hook shim, and forwards everything else
status: active
guards:
  - cli/dira/src/headless.rs
checks:
  - the headless switch, GitHub Actions detection, and which hooks are human :: cargo test -p dira --bin dira -- headless::tests
  - a headless prompt is never forwarded, agent hooks still are, and DIRA_HEADLESS overrides both ways :: cargo test -p dira --test headless_e2e
origin: session
verified: true
---

## Decision

`dira hook` treats a run as **headless** when `DIRA_HEADLESS` is set to anything other than
`0`/`false`/`no`/`off`, or, with `DIRA_HEADLESS` unset, when `GITHUB_ACTIONS=true`. In a headless
run the shim asks the same normalizer the daemon uses (`dira_sources::normalize_for`) whether the
hook is a human signal (`EventKind::is_human_signal`: a prompt, a permission request or
notification) and, if it is, exits 0 without forwarding it (`HookOutcome::Withheld`). Every other
hook forwards unchanged. `dira doctor --probe` never withholds.

## Why

CI agents (`claude-code-action`, the dodi-smart factory) submit their prompt through
`UserPromptSubmit` and receive `Notification`s, the same hooks a person produces. Each maps to a
human signal, so the daemon opened human-engaged intervals for runs nobody was at: prod showed
factory sessions on `github-actions` devices with 254–292 s of human time running past the
session's end. That is false billable time on a run that is, by construction, agent-only.

The decision has to be made in the shim because it is the only process that sees the agent's own
environment. The daemon is shared and long-lived; on a laptop it serves interactive sessions
next to anything else, so an env read there would describe the daemon's launch, not the run.

Withholding is a capture fact, not a billing policy: nothing is re-weighted. A signal that did not
come from a person is not reported as one. The session, its agent wall-clock, tool spans, tokens
and sync are untouched, because all of those ride non-human hooks. The cost is that a headless
session reports `prompts: 0`, which is accurate for "human prompts observed".

`GITHUB_ACTIONS=true` is documented by GitHub as set in every Actions job and is never set on a
workstation, and no person can type into a hosted job's agent, so it is a safe default.
`DIRA_HEADLESS` covers every other headless runner and lets a self-hosted setup opt out.

## Known limits

- `GITHUB_ACTIONS=true` is also set on self-hosted runners, under `act`, and in an
  `action-tmate` debug shell, where a person can be typing. Those set `DIRA_HEADLESS=0`.
- The opencode plugin posts straight to the daemon's `/hooks/{harness}` route and never runs
  the shim, so opencode in CI still reports prompts. Covering it needs an env check in the
  plugin; nothing in the factory runs opencode today.
- A headless session killed before its first tool call or `Stop` has no signal left and is
  pruned as degenerate. It has no agent time either, so nothing billable is lost.

## Rejected

- **A new `EventKind` for headless prompts** — keeps the prompt count, but adds a stored enum value
  an older binary cannot read (DIRASH-0035 refuses it loudly), for a count nobody bills on.
- **Daemon-side filtering via a forwarded `dira_env.headless` marker** — needs a matching daemon and
  shim, and still makes the decision from data only the shim has. Withholding at the shim needs no
  protocol change.
- **Inferring headless from `DIRA_RUNTIME` or the cloud runtime id** — runtime detection is
  labelling only (`runtime.rs`), and Claude Code on the web is a cloud runtime with a person at the
  keyboard.
- **Zeroing human time in the cloud for runner devices** — runner-token devices also back
  interactive cloud sessions (`cloud:claude-web:*`); the cloud cannot tell the two apart.

## Agent directives

- Keep `headless::decide` pure over an injected env lookup; read the process env only in
  `is_headless`.
- Classify through `dira_sources::normalize_for` and `EventKind::is_human_signal`. Never keep a
  separate list of "human" hook names in the shim: it would drift from the daemon's mapping.
- Withhold only human signals. Never drop `SessionStart`, tool, `Stop` or `SessionEnd` hooks in a
  headless run, or the agent-only session the factory measures disappears with them.
- Keep the `!probe` guard in `main.rs`'s `forward_hook`: the capture probe sends a
  `UserPromptSubmit` and must land even when `dira doctor` runs in CI (DIRASH-0023).
- A withheld hook stays silent: no `hook_health` write, one line under `DIRA_HOOK_DEBUG`.
