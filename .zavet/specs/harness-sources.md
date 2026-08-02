---
title: Harness sources and hook ingestion
version: 2
origin: session
verified: false
confidence: high
date: 2026-07-31
paths:
  - cli/sources/src/
  - cli/dira/src/init.rs
  - contract/src/lib.rs
decisions: [D-0001]
---

## Overview

How AI coding harnesses report to dira: each harness pushes lifecycle hooks
(`dira hook <id>` over the stdin→socket shim, or HTTP `POST /hooks/{id}`), and
a per-harness source in `cli/sources` normalizes the payload into the shared
`EventKind` vocabulary. No log tailing; the daemon stamps arrival timestamps.

## Behavior

- Supported sources: claude (default), codex, gemini, cursor, opencode
  (HTTP plugin), grok (grok-build), generic (harness-neutral JSON). `Manual`
  is a pseudo-harness for `dira start`, outside the sources crate.
- Alias spelling lives ONLY in `canonical_harness_id()` — `dira init` and hook
  dispatch both resolve through it so accepted spellings can never drift.
- A source returns `None` for unknown events or malformed payloads; the daemon
  acks silently so a harness never retries. This is also the cross-harness
  defense: grok-build can replay Claude/Cursor hook configs (compat layer),
  and those camelCase envelopes simply normalize to `None` in the other
  sources.
- `dira init grok` writes a dedicated `~/.grok/hooks/dira.json` (grok-build
  merges every `*.json` in `~/.grok/hooks/`; user scope is always trusted,
  project scope would prompt for folder trust). Grok's envelope is camelCase
  (`hookEventName`, `sessionId`, `workspaceRoot`) with snake_case event
  values (`user_prompt_submit`); `cwd` falls back to `workspaceRoot`.
- Grok's `transcriptPath` (an ACP `updates.jsonl`) is forwarded through
  `Normalized` like any other harness's transcript path. The daemon's
  `capture_tokens` (`cli/dirad/src/writer.rs`) selects a harness-specific
  parser: `Harness::Grok` uses `dira_core::tokens::parse_grok_updates_usage`,
  which reads `_x.ai/session/update` envelopes and extracts `usage` only from
  `turn_completed` records, keyed by the update's `_meta.eventId` (falling
  back to `prompt_id` + envelope timestamp). The offset/dedup machinery is
  shared with Claude's transcript capture — only the per-line parser differs.
  `signals.json` was investigated as an alternative (diffing its counters)
  and dropped: it carries no token counters at all, so `updates.jsonl` is the
  only source of per-turn usage.

## Interfaces & data

- `HarnessSource` trait: `harness()` (contract enum tag), `id()` (wire id),
  `normalize(payload) -> Option<Normalized>`. Registered in `registry()`.
- `Normalized` carries only metadata: session_id, kind, cwd, tool name,
  optional transcript path, and the optional lifecycle reasons `source` /
  `reason`. Prompt text, tool arguments, and outputs are never read (D-0001
  posture extends to ingestion).
- `source` / `reason` are Claude Code's `SessionStart.source` (`startup`,
  `resume`, `clear`, `compact`, `fork`) and `SessionEnd.reason` (`clear`,
  `resume`, `logout`, `prompt_input_exit`, `bypass_permissions_disabled`,
  `other`). Every other source sets both `None`. Nothing accounts on them —
  they ride the writer's ingest debug line. They exist because dropping them
  made a launcher spawn and a real session indistinguishable at the ingress,
  which is what kept issue #74 invisible; the fix for that issue gates on
  observed activity instead, so it stays harness-independent.
- Adding a harness touches: contract `Harness` enum (+ `just contract`),
  the source module + `lib.rs` registry/aliases, `init.rs` writer,
  `main.rs` init dispatch/help, README table. The daemon needs no changes.

## Invariants

- Only metadata crosses ingestion; hook payload content fields are ignored.
- Every unknown event/payload is a silent ack — never an error to the
  harness, never a retry loop.
- `dira hook <id>` always exits 0 and writes nothing to stdout. That is the
  *harness* contract, and it is unchanged. A **transport** failure (the daemon
  could not be reached, or refused the connection) additionally leaves a durable
  local breadcrumb that `dira status` surfaces — "never tell the harness" had
  been implemented as "never tell anyone", so a dead capture channel was
  indistinguishable from a healthy one for days. A **semantic** non-result
  (unknown harness, unaccounted event kind) stays silent everywhere. See D-0016.
- The contract `Harness` enum is the wire's source of truth; schema artifacts
  are regenerated, never hand-edited.

## Open Questions

- Should `dira init` warn when grok-build's Claude/Cursor compat replay is
  enabled (double-fire is harmless but noisy in daemon logs)?
