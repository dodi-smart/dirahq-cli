---
title: The dira Claude Code mod
version: 1
origin: session
verified: false
confidence: medium
date: 2026-10-13
paths:
  - mods/dira/**
  - .claude-plugin/marketplace.json
decisions: [D-0001, D-0008]
---

## Overview

A Claude Code mod (function hooks, `mods/dira/`) that surfaces dira, zavet and
a local proba forecast inside Claude Code itself. It reads only what the
installed tools print: `dira status --json`, `dira zavet decisions --json`, the
installed zavet plugin's `zavet specs --json`, `zavet list`, `guards`,
`spec-match`, `audit`, `check`, and `gh pr view`. It never writes to dira's
store or the cloud.

## Behavior

- Quiet by default: no hook holds a tool call, and nothing asks a question on
  its own. Alert-style bands (edit gates, commit holds, health nags) were tried
  and removed as too disruptive.
- Status line: this session's engaged and agent time from dira, this session's
  cost from Claude Code's own usage, the billing period's unbilled amount, and
  the decisions/specs touched this session. A 24-bit copy is written to
  `~/.cache/dira/mod/status-<session>.ansi` (falling back to
  `/tmp/claude-<uid>/dira-mod`) for `bin/statusline.sh`.
- Guard knowledge is matched locally against `zavet guards` globs. When the
  model reads a guarded file, the decision's directive is appended to that
  conversation once (per subagent, too), and the main session also records it
  in zavet's own `zavet-shown-<session>` file so zavet's deny-and-retry does
  not fire a second time.
- The proba forecast mirrors the cloud's additive ladder (585: ci + review;
  750: + authorship + supervision; 999: + intent) from local evidence only. It
  is labelled a forecast; the cloud grade is authoritative. The band shows only
  for a branch with an open PR.
- `/vouch` hands the model an interview prompt. `verified: true` is set only on
  the person's explicit confirmation, and decision records stay append-only
  for meaning (a changed claim becomes a correcting record).
- Work-item focus applies to framework units (OpenSpec, Spec Kit, BMAD) only,
  never to zavet's living specs.
- `/dira-mods` switches the mod, or one part of it (status, composer,
  transcript, band, steward), on or off; the choice is kept in the plugin
  store across sessions. An off status part writes an empty line, which
  `bin/statusline.sh` prints as nothing.
- A refused command name (Claude Code took `/focus` as a built-in) must not
  cost the rest: every registration fails on its own.

## Open Questions

- zavet's honesty rules do not list `verified: false → true` among the allowed
  decision-record mutations; `/vouch` asks before flipping it on a decision.
- Cloud has no device-readable proba/miara API yet, so the forecast is local.
