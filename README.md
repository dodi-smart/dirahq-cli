# Dira — CLI & contract (open source)

**AI-first time tracker — *"if you can clone it, you can bill it."***

This repository is the open-source surface of **Dira**: the command-line client, the
capture daemon, and the wire contract. It is published under the
[Apache License 2.0](LICENSE).

Dira captures how you actually spend time supervising AI coding agents — human-engaged
minutes, agent wall-clock, token usage, and the git artifacts behind the work — as
**policy-free, metadata-only proofs**. Billing is resolved later, in the cloud, against
effective-dated policy.

- **Capture is policy-free & metadata-only.** Durations, counts, SHAs, session ids, tool
  names — never prompt text, file contents, or diffs. Open so you can audit exactly that.
- **Human time is de-duplicated** across concurrent sessions: one person supervising three
  agents is still one human-minute per minute. Agent wall-clock sums freely.
- **Offline-first.** Everything works with no network and no account; the cloud is an
  optional sync target.

## What's here

```
/contract     Rust source-of-truth wire schema (serde + schemars) → attestation.schema.json
/cli
  /core       capture engine: event model, accounting FSM, store, project resolver, report
  /dirad     resident daemon (tokio): ingress (loopback HTTP + UDS), accounting, store
  /dira      thin CLI client over the daemon's Unix domain socket
  /sources    per-harness hook normalization (claude_code, …)
mise.toml     toolchain pins (rust, just)
justfile      task runner
```

## What's NOT here

The **Dira cloud** — the hosted verify/billing/policy/dashboard service — is a proprietary
service in a separate repository and is not part of this project.

The **[zavet plugin](https://github.com/dodi-smart/dirahq-zavet)** — the repo-local
knowledge layer (decision records, guard hooks, commit-trailer conventions) — is its own
product in its own repository. dira ships the optional integration only: repos that carry
`.zavet/` get their decisions, trailers, and guard events captured and correlated with
session time (`dira zavet why D-0042` = the decision *and* what it cost). See
[docs/zavet.md](docs/zavet.md).

## Quick start

```sh
mise install                 # rust + just
just test                    # unit + property tests for the accounting invariants

# Build the release binaries, symlink `dira` + `dirad` into ~/.local/bin
# (must be on your PATH), and restart the daemon onto the fresh build:
just install                 # build + link globally + restart daemon

dira init                   # wire Claude Code hooks for this repo
dira status                 # active sessions + today's human vs agent time
dira start --label meeting  # a manual dira (several may run at once)
dira stop --all
dira log 45 --note "review" # retroactive entry (bare number = minutes)
dira report --week
```

> The symlink target dir defaults to `~/.local/bin`; override with `DIRA_BIN_DIR`.

`dira init` writes Claude Code **command hooks** into `.claude/settings.json`; each event
runs `dira hook claude`, which forwards the payload to the daemon over the socket. The hot
path only does a non-blocking enqueue, so the agent loop never waits on us.

Other harnesses are wired the same way — `dira init <harness>`:

| Harness | `dira init …` | How it's wired |
|---|---|---|
| Claude Code | `dira init` (default) | command hooks → `.claude/settings.json` |
| Codex | `dira init codex` | prints `~/.codex/config.toml` `[[hooks.…]]` snippet to paste |
| Gemini CLI | `dira init gemini` | command hooks → `~/.gemini/settings.json` |
| Cursor | `dira init cursor` | command hooks → `~/.cursor/hooks.json` |
| OpenCode | `dira init opencode` | forwarder plugin → `~/.config/opencode/plugin/dira.js` (HTTP) |

The command-hook harnesses (Claude, Codex, Gemini, Cursor) all forward over the same
stdin→socket shim (`dira hook <harness>`); OpenCode has no command hooks, so it POSTs to the
daemon's loopback `/hooks/opencode` route instead. Each harness's own hook vocabulary is
normalized into Dira's shared event set in `cli/sources`.

## Cloud sync (optional)

The CLI/daemon point at the hosted cloud (`https://app.dirahq.sh`) out of the box, but
**nothing is ever sent until you link the device** — unlinked, dira is fully offline.

```sh
dira device link             # enter the one-time code from the dashboard's Connections screen
```

The URL is ordinary layered config (defaults → `config.toml` → `DIRA_*` env, env wins),
so pointing a checkout at a local or self-hosted cloud is one line:

```sh
DIRA_CLOUD_URL=http://localhost:3000 dira device link --code LOCALDEV1   # per-invocation
dira config set cloud_url http://localhost:3000                          # persistent
```

## Contract

The wire schema is authored once in Rust (`/contract`) because the daemon is the producer.
`just contract` emits `contract/attestation.schema.json` and the deterministic signing
fixture `contract/testdata/signing-vector.json`. Both are drift-gated in CI; never hand-edit
them. The cloud consumes them by vendoring.

## Contributing

Contributions are accepted under the DCO (`git commit -s`) and licensed Apache-2.0. See
[CONTRIBUTING.md](CONTRIBUTING.md). "Dira" and the Dira logo are trademarks of Dodi Smart
OOD; the Apache-2.0 license does not grant trademark rights.

## Notes

- Rust is managed by `mise`; run cargo via `mise exec -- cargo …` if `mise` isn't shell-activated.
- The accounting invariants (no double-count, idle-trim) are property-tested against random
  interleaved multi-session event streams — see `cli/core/src/accounting.rs`.
