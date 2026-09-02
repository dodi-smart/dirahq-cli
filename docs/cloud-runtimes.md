# Dira in cloud agent runtimes

Coding agents no longer run only on your machine. Claude Code on the web runs
sessions in ephemeral cloud VMs; Cursor's cloud agents do the same. Work done
there is real work — agent wall-clock, tokens, commits — and without
instrumentation it simply vanishes from your record.

`dira cloud init` "teleports" dira into those VMs: it generates portable,
repo-committed artifacts that install dira inside the session VM, capture the
harness's hook events, and sync signed attestations to your Dira cloud — the
same metadata-only proofs your laptop produces.

## How it works

```
git repo (committed)                    cloud VM (per session)
─────────────────────                   ─────────────────────────────
.claude/settings.json ── SessionStart ─▶ .dira/bootstrap.sh
.cursor/hooks.json                        ├─ downloads pinned dira release
.dira/hook.sh                             │    (sha256-verified, GitHub)
.dira/bootstrap.sh                        ├─ starts dirad
                                          ├─ dira device link --runner-token
                                          │    (ephemeral per-VM device)
                                          └─ forwards the event ─▶ dirad
         every other hook event ──▶ .dira/hook.sh ──▶ dirad
                                          └─ signed batches ─▶ your Dira cloud
```

Everything is repo-delivered because that is the only channel an ephemeral VM
reliably gets: cloud sessions clone your repository and honor its committed
hook configs (`.claude/settings.json`, `.cursor/hooks.json`). The generated
commands are **portable** — they invoke `.dira/hook.sh`, which resolves the
`dira` binary at run time — unlike `dira init`'s local configs, which embed
this machine's absolute path.

On your laptop the same files are harmless: `bootstrap.sh` detects it is not
in a cloud runtime and skips straight to forwarding, and `hook.sh` silently
drops events on machines with no dira installed. One committed config, correct
everywhere. If you've also run `dira init` on that laptop (ordinary onboarding
wires hooks at your user scope too), the portable path yields instead of
delivering the same event twice: `.dira/hook.sh` marks its own invocation, and
`dira hook` checks whether your live, machine-specific wiring will already
deliver this event before forwarding through the portable one.

## Setup

### 1. Wire the repo

```console
$ dira cloud init            # claude + cursor
$ git add .dira .claude .cursor && git commit -s -m "chore(repo): wire dira cloud capture"
```

Re-run it any time (idempotent); it also upgrades machine-specific hook
entries a plain `dira init` left in the project files. It writes at the git
repository's toplevel (not the working directory), so run it from anywhere
inside the repo; outside a git work tree it refuses with an actionable message
unless you pass `--print`, which prints everything without touching the
filesystem. `dira init`, run afterward, recognizes the portable wrapper as
already wiring the project and adds nothing there — it prints a note instead
— while `dira init --global` still wires your own machine as usual.

`dira cloud init` also fetches and embeds a sha256 digest for each release
target from the generating binary's own version, so the bootstrap verifies
the download against a pin baked in at commit time rather than a second
network fetch at install time; pass `--no-pin` to skip that and let the
bootstrap fall back to the release's own `.sha256` asset instead. A failed
fetch never blocks the command — it warns and writes the unpinned form (or
keeps an existing pin, if the version didn't change).

Two more best-effort warnings can show up here, and both are worth reading
rather than dismissing:

- **Windows**: the committed command needs Git Bash's `sh` to run at all —
  `dira cloud init` only warns about this on the machine that *generates* the
  config, so if you commit from macOS/Linux and a teammate clones onto
  Windows, they get no warning even though the same requirement applies to
  them too.
- **A `.gitignore`d artifact silently never ships.** `dira cloud init` runs
  `git check-ignore` on everything it just wrote (`.dira/hook.sh`,
  `.dira/bootstrap.sh`, `.dira/.gitattributes`, and each harness's hook
  config) and warns with the path when one matches an ignore rule — as
  written it will never be committed, so a cloud VM cloning the repo never
  gets it. The fix is to add a negation rule (`!.dira/`, or the specific
  path) or remove the matching rule, then `git add` it.

### 2. Mint a runner token

On the dashboard's **Connections** page, mint a **runner token** (one per
cloud environment is good hygiene). A runner token is the headless sibling of
the one-time link code: the VM presents it to
`POST /api/v1/devices/claim` and receives a fresh, ephemeral, server-assigned
device identity — no keyboard, no keychain. Tokens are revocable; treat them
like any credential.

> Runner tokens require a Dira cloud that implements the runner-claim variant
> (see `.zavet/specs/cloud-runtime.md`). Without one, cloud sessions still
> capture locally but have nothing durable to sync to — the VM's store dies
> with the VM.

### 3. Configure the cloud environment

**Claude Code on the web** (environment settings at claude.ai/code):

- **Network access**: *Custom*, allowing your Dira cloud host (default
  `app.dirahq.sh`) alongside the default package-registry list. GitHub
  release assets — where the bootstrap downloads dira from — are already on
  the default allowlist. This step is **not optional**: measured inside a
  default cloud environment, `app.dirahq.sh` is refused at the proxy with
  `CONNECT tunnel failed, response 403` before TLS is even negotiated. Until
  the host is allowed, cloud sessions capture locally and sync nothing.
- **Environment variables**:

  ```
  DIRA_RUNNER_TOKEN=<runner token>
  DIRA_IDENTITY_EMAIL=<the email this work is attributed to>
  ```

  Anyone who can use the environment can read its variables, so prefer
  personal environments, and prefer runner tokens (revocable, blast radius =
  one workspace's ephemeral devices) over ever pasting a real device secret.
- **Setup script** (optional fast lane): the bootstrap self-installs in a few
  seconds, but a setup script's result is snapshot-cached across sessions.
  `dira cloud init` prints a ready-made snippet.

**Cursor cloud agents** (`.cursor/environment.json`, committed):

```json
{
  "install": "sh .dira/bootstrap.sh --install-only",
  "start": "sh .dira/bootstrap.sh --provision-only",
  "env": { "DIRA_RUNTIME": "cursor-cloud" }
}
```

Cursor runs `install` once per build (the result is cached on disk) and
`start` on every machine boot, so `start` is what actually brings the daemon
up. Provisioning deliberately does **not** hang off a hook here, for two
reasons: Cursor cloud agents do not run `sessionStart`/`sessionEnd` — Cursor's
own docs list both among the hooks a local run supports, but name them
explicitly unavailable to cloud agents — and every harness's hooks only start
once the agent has a writable environment regardless, so provisioning cannot
hang a build phase off one either way. Capture begins once the environment is
writable.

`dira cloud init` still writes `.cursor/hooks.json`; Cursor cloud agents do
run command hooks from it, and that is what feeds the daemon once it is up.

Set `DIRA_RUNNER_TOKEN` / `DIRA_IDENTITY_EMAIL` as environment secrets.
`DIRA_RUNTIME` is how the VM declares itself, since Cursor publishes no
in-VM marker dira can detect on its own.

### 4. Verify

Start a cloud session and ask the agent to run:

```console
$ dira doctor --json
```

Three checks cover the cloud path: `cloud.runtime` (runtime detected,
provisioning env present; `skip` outside a cloud runtime — there's nothing to
have evaluated), `cloud.reachability` (one GET against your cloud's
`/api/v1/meta` — a TLS failure points at `DIRA_EXTRA_CA_CERTS`, anything else
at the network allowlist; this one only fires its GET automatically inside a
detected cloud runtime, which a cloud session is, so it runs here — on your
own machine it stays `skip` unless you name it with
`dira doctor --check cloud.reachability`), and `cloud.bootstrap` (committed
artifacts whole, version pin readable). None of the three ever fails the
command — a missing runner token or an unreachable cloud is a warning, not
lost local capture. `dira status` should show the session; the dashboard
shows the ephemeral device labeled `cloud:claude-web:<session>`.

For a scriptable answer to "did dira capture anything", ask for JSON — it
carries today's rollup, so it still reports sessions that have already ended:

```console
$ dira status --json | jq '.today | {sessions: .session_count, agent: .total_agent_seconds}'
{ "sessions": 1, "agent": 4 }
```

`--json` keeps its one-object-on-stdout contract on failure too: a daemon that
answers with an error still emits `{"status":"error","message":…}` on stdout
(exit 1), so a script never has to special-case a human `error: …` line
landing on stderr instead. A daemon that is not reachable at all is a
different, earlier failure — the connect itself fails before there is any
response to render — so that case goes to stderr with exit 1 and prints no
JSON.

### TLS through the egress proxy

Cloud runtimes re-terminate every outbound TLS connection at a security proxy,
and dira trusts only the roots compiled into its binary (decision D-0011), so
without the proxy's CA **every** HTTPS call fails:

```
cannot reach https://app.dirahq.sh: … invalid peer certificate: UnknownIssuer
```

The bootstrap handles this automatically by promoting the CA the runtime
declares into `DIRA_EXTRA_CA_CERTS`. There is nothing to configure in the
runtime's settings for this — the CA is provided by the environment, and
`dira doctor`'s `cloud.reachability` check names the fix when it is missing.
The network allowlist above is a separate matter, and that one *does* need a
setting.

## What the cloud pieces are

| Piece | What it does |
|---|---|
| `.dira/hook.sh` | Portable forwarder every hook event runs. Resolves `dira` at run time; always exits 0; drops events silently where dira isn't installed. Marks its own invocation (`DIRA_HOOK_VIA=portable`) so `dira hook` can yield to a live user-scope wiring for the same event instead of delivering it twice on an onboarded laptop. |
| `.dira/bootstrap.sh` | The teleport: in a cloud VM, installs the release (sha256-verified against an embedded digest, or the release's own `.sha256` asset), starts `dirad`, claims a runner-token device, then forwards the event; the SessionStart entry carries a 300s hook timeout. Also `--install-only` (build phase; install only, **exits non-zero if no binary ends up installed**) and `--provision-only` (boot phase: provision, start the daemon, forward nothing, always exits 0) for runtimes that provision from their environment config rather than a hook. |
| `DIRA_RUNNER_TOKEN` | Headless device claim. Revocable, workspace-scoped, mints ephemeral per-VM devices. Never appears on any subprocess's argv — the bootstrap's `dira device link` call reads it straight from the environment. |
| `DIRA_IDENTITY_EMAIL` | Attribution override — the VM's `git config user.email` belongs to the platform bot, not to you. Must contain `@` and stay under 255 chars; an implausible value is ignored (with a warning) rather than attributing work to garbage. |
| `DIRA_EXTRA_CA_CERTS` | Additive PEM trust anchors for the runtime's TLS-intercepting egress proxy. The bootstrap sets it from the CA the runtime declares (`$SSL_CERT_FILE`, `$NODE_EXTRA_CA_CERTS`, `~/.ccr/ca-bundle.crt`, then the system bundle). Never replaces the bundled roots, and a bundle with one corrupt block still loads the certs that do parse — see decision DIRASH-0033. |
| `DIRA_SYNC_BACKSTOP_SECS` | Sync safety-net cadence; bootstrap sets `15` so an abruptly reclaimed VM loses at most ~15s of un-synced tail. Batches are idempotent, so nothing double-counts. |
| `DIRA_RUNTIME` | Explicit runtime marker (`cursor-cloud`, …) for runtimes without a detectable one. Trimmed and clamped to 64 chars before it reaches a runner label or the wire. |

## How this stays working

Most of the cloud path is covered by ordinary tests, but the part that matters
most — that a harness nobody controls, wired only through committed config,
fires hooks that land as counted events — cannot be proved by a test that
fakes the harness. So CI runs the real thing:
[`.github/scripts/cloud-capture-smoke.sh`](../.github/scripts/cloud-capture-smoke.sh)
wires a throwaway repo with `dira cloud init --no-pin`, sets
`CLAUDE_CODE_REMOTE=true` to drive the same bootstrap branch a real cloud
session takes, runs Claude Code headless against it, and then asserts three
things: the daemon is running — the strongest available signal that the
bootstrap's own `SessionStart` forward specifically is what got things going,
since `.dira/bootstrap.sh` is the only generated command wired to
`SessionStart` and nothing else in the harness's wiring starts the daemon —
`dira status --json` shows a captured session, and that session was
attributed to the fixture repo's canonical ref (which proves the writer's git
enrichment ran, not merely that an event arrived).

The workflow is [`cloud-capture.yml`](../.github/workflows/cloud-capture.yml).
It needs a `CLAUDE_CODE_OAUTH_TOKEN` repository secret (generate one with
`claude setup-token`); without it — on fork PRs, where GitHub exposes no
secrets — the script exits 0 with a `SKIP`, so a contributor never sees a red
check they cannot fix. The run is bounded by `--max-budget-usd` and scoped to
PRs that touch the cloud path, since it spends real tokens. You can run it
locally too:

```console
$ cargo build -p dira -p dirad
$ CLAUDE_CODE_OAUTH_TOKEN=… sh .github/scripts/cloud-capture-smoke.sh
cloud-capture: PASS — sessions=1 agent_seconds=4 project=github.com/dira-smoke/cloud-capture
```

It redirects every path dira writes into a temp dir, so it cannot touch your
real store, socket, or cache.

## What to expect in the data

- **Agent time and tokens** are captured exactly as locally. **Human time**
  in a cloud session is usually near zero: prompts you send from the web UI
  do arrive as `UserPromptSubmit` hooks, but most cloud sessions are
  agent-driven. An agent-only session is first-class in dira's model.
- Devices are **ephemeral**: one per VM, labeled with the runtime and session
  reference, dying with the VM. The cloud can garbage-collect inactive ones.
- The wire stays **content-free** (D-0001): no prompts, diffs, or file bodies
  leave the VM — the same contract as everywhere else.

## Limitations and roadmap

- The runner-claim endpoint ships in the CLI ahead of the cloud; until your
  cloud supports it, cloud VMs capture without syncing.
- Session rollups carry the runtime on the wire since schema 1.4:
  `SessionRollup.runtime` (`claude-web`, `cursor-cloud`, …) and
  `runtimeSessionRef` (the harness's own session id, for transcript
  deep-links) are stamped by the daemon at flush time whenever
  `dira_core::runtime::detect` finds a cloud runtime, and omitted entirely
  otherwise — local payloads are byte-identical to 1.3. A cloud on an older
  vendored schema simply ignores the two fields (additive-minor rule).
- Cursor cloud agents do run `.cursor/hooks.json` command hooks, but not
  during the agent's early read-only turns, so the first exploratory phase of
  a cloud agent goes uncaptured. Claude Code on the web remains the reference
  runtime — it is the one the CI smoke test exercises on every change.
- Grok Build has hooks and a headless mode but no vendor-hosted cloud runtime
  to teleport into, so it needs nothing here; `dira init grok` already covers
  it wherever it runs.
- An alternative to runner tokens — provisioning a pre-linked device via
  `DIRA_DEVICE_SECRET` — works for CI-style setups you fully control, but is
  deliberately not the documented path for shared cloud environments: a
  static signing key in environment variables is a worse trade than a
  revocable token.
