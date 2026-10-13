# dira mod for Claude Code

Brings dira (time), zavet (decisions and living specs) and a local proba
forecast (PR hallmark) into Claude Code itself. Every surface lights up only
when its source is configured: dira when the `dira` binary answers, zavet when
the repo has `.zavet/` and the zavet plugin is installed.

Requires Claude Code 2.1.287 or later (function hooks).

## Install

```
/plugin install dira --marketplace dodi-smart/dirahq-cli
```

For the coloured status line, point Claude Code's `statusLine` at the script
the plugin ships, e.g. in `~/.claude/settings.json`:

```json
"statusLine": { "type": "command", "command": "sh <plugin dir>/bin/statusline.sh", "padding": 0 }
```

Without it the mod pins a plain-text copy of the same line.

## What it does

| Surface | What you get |
|---|---|
| Status line | `● you ◆ agent · $ session · ▲ unbilled · ◇ decisions ▪ specs` for this session |
| Prompt box | guarded paths, decision ids and spec slugs light up as you type and travel with the prompt; a phrase that contradicts a decision is underlined |
| Transcript | knowledge chips under Edit/Write/commit/PR rows, decision and spec titles under replies, a knowledge check under plans, briefed subagents |
| Band | only for a branch with an open PR: the proba forecast `⬡ 750 ●●●●○` (hover for details) and a map of changed files ↔ decisions and specs |

Commands: `/desk` (knowledge board), `/vouch [id|slug]` (interview to finish
unverified records — only you mark one verified), `/dira-focus`,
`/dira-mods [on|off] | <part> <on|off>` (switch the mod or one part —
status, composer, transcript, band, steward — remembered across sessions),
`/dira-demo [off]`, `/dira-doctor`.

The proba forecast is computed locally (gh, dira, `zavet check`); proba in
dira cloud grades the merged PR.

## Develop

```
claude plugin validate mods/dira
CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude plugin test mods/dira
claude --plugin-dir mods/dira
```
