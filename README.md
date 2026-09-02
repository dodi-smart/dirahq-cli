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
/docs         docs/install.md (installer reference), docs/zavet.md (knowledge module)
install.sh    curl | sh installer for dira + dirad (see docs/install.md)
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

## Install

```sh
curl -fsSL https://dirahq.sh/install | sh
```

Installs `dira` + `dirad` into `~/.local/bin` (override with `DIRA_BIN_DIR`). Targets:
macOS (universal — Apple Silicon and Intel, one download) and Linux x86_64/arm64 (static
musl — works on Alpine and old glibc alike); Windows via WSL2.

Then set the machine up:

```sh
dira onboard
```

One command: detects your harnesses and wires them all, registers `dirad` as a login
service so it survives reboots, offers to link this device, and installs the zavet
knowledge layer. Every step is skippable, and re-running it is safe — it reports what is
already done and picks up the rest. `dira onboard --yes` accepts every default without
prompting; `--print` shows the plan and changes nothing. See
[docs/getting-started.md](docs/getting-started.md).

```sh
dira status           # today's summary — engaged, agent, compute, unbilled
dira doctor           # is capture actually working? (add --probe to prove it end to end)
```

Prefer to do it yourself? Every step `onboard` runs is its own command — `dira init`,
`dira daemon install`, `dira device link`, `dira zavet install`.

Stay current with `dira update` — sha256-verified, atomic, restarts the daemon for you.
See [docs/install.md](docs/install.md) for every flag/env var, air-gapped installs, and
troubleshooting.

## Build from source

The **contributor path** — build dira yourself instead of downloading a release.

```sh
mise install                 # rust + just
just test                    # unit + property tests for the accounting invariants
just install                 # build release binaries, symlink onto PATH, restart daemon
```

`just install` symlinks `target/release/{dira,dirad}` onto `$DIRA_BIN_DIR` (default
`~/.local/bin`) for a fast dogfood loop — it is **not** a real install. `dira update`
deliberately refuses to touch a `just install` dev symlink (see
[docs/install.md](docs/install.md)); re-run `just install` instead to pick up a new build.

```sh
dira start --label meeting  # a manual session (several may run at once)
dira stop --all
dira log 45 --note "review" # retroactive entry (bare number = minutes)
dira report --week
```

## Wiring other harnesses

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
| Grok Build | `dira init grok` | command hooks → `~/.grok/hooks/dira.json` |

The command-hook harnesses (Claude, Codex, Gemini, Cursor, Grok Build) all forward over the same
stdin→socket shim (`dira hook <harness>`); OpenCode has no command hooks, so it POSTs to the
daemon's loopback `/hooks/opencode` route instead. Each harness's own hook vocabulary is
normalized into Dira's shared event set in `cli/sources`.

### Cloud agent runtimes

Agents that run in the cloud — Claude Code on the web, Cursor cloud agents — get captured
too: `dira cloud init` generates portable, repo-committed wiring (`.dira/hook.sh`,
`.dira/bootstrap.sh`, project hook configs) that installs dira inside each session VM,
captures its hook events, and syncs attestations from there. The generated bootstrap ships
with a sha256 digest pinned at commit time (`--no-pin` to opt out), so a session VM verifies
its own download without a second network round trip. See
[docs/cloud-runtimes.md](docs/cloud-runtimes.md).

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

- Output honours `NO_COLOR` (no SGR escapes) and `DIRA_ASCII=1` (no non-ASCII glyphs —
  `*` `+` `~` for the metric marks, `#`/`.` for bars). They are independent: a console that
  cannot draw `█` is not the same thing as a pipe. On Windows the ASCII set also turns on
  automatically when the console code page is not UTF-8, and the CLI asks the console to
  interpret ANSI escapes at startup so legacy `conhost` does not print them literally.
- Rust is managed by `mise`; run cargo via `mise exec -- cargo …` if `mise` isn't shell-activated.
- The accounting invariants (no double-count, idle-trim) are property-tested against random
  interleaved multi-session event streams — see `cli/core/src/accounting.rs`.
