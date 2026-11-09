# Dira CLI: transparent install, silent auto-update, zero event loss (+ contract 2.0)

> On approval this file is copied to `docs/superpowers/plans/2026-11-09-cli-simplify-install-update.md`
> and the contract 2.0 section to the shared notes folder for the cloud-redesign session.

## Context

Windows users report three problems:
- an empty console window stays open after every update;
- the daemon does not start after install or reboot;
- a large backlog of events never gets handled.

The user wants Dira to be nearly invisible: an admin-free install, background auto-update like
Claude Code, one-command onboarding, and no missed events. They also approved a breaking wire
**contract 2.0** (the cloud accepts 1.x through adapters for a window). This session owns the
CLI side of that contract.

User decisions (2026-11-09):
- Test builds: cross-compile on the Mac with `cargo-xwin`.
- Auto-apply updates by default.
- Register the per-user service by default, even without a terminal.
- Release signing goes in a follow-up PR.

## Findings (code evidence; Windows repro pending, see Phase 0)

| # | Symptom | Root cause | Where |
|---|---|---|---|
| F1 | Empty console after update and at every logon | `dirad.exe` is a console-subsystem binary (no `windows_subsystem` anywhere). When Task Scheduler (`schtasks /Run`, which `daemon::restart` calls after every update) or the Run key starts it, Windows gives it a new console. Logging goes to a file, so the window stays empty. Closing it triggers `ctrl_close`, which shuts dirad down. | `cli/dirad/src/main.rs`, `cli/dira/src/daemon.rs:1632`, `dirad/src/lib.rs:636` |
| F2 | Service not started | `schtasks /Create /SC ONLOGON` without a user ID needs admin, so standard users fall through to the HKCU Run key. That fallback **returns Ok without starting dirad**, so nothing runs until the next logon. If the task is created, it gets the defaults: no start on battery, stopped when unplugged, killed after 72 h, never restarted on failure. | `daemon.rs:1025-1090` |
| F3 | Daemon dead after `dira update` | Supervision detection maps the Run key to `ScheduledTask`. Restart then shuts dirad down and runs `schtasks /Run` against a task that doesn't exist. The update rolls back and the daemon stays down. | `daemon.rs:1423-1444`, `:1557-1643`, `update/mod.rs:370` |
| F4 | Old dirad survives a reinstall or uninstall | Supervised dirad has no pidfile, so `daemon stop` does nothing. Uninstall only ends the task and leaves a Run-key dirad running. | `daemon.rs:351-360`, `:1143` |
| F5 | **Events dropped silently** | A full ingest queue returns `Response::Ok` (`Ok(()) \| Err(Full(_)) => Response::Ok`). The shim records success and the event is gone, with no log or counter. | `dirad/src/control.rs:327-328` |
| F6 | Queue fills on Windows | The writer runs `enrich` inline. That spawns `git rev-parse` on every event with a cwd, plus about 5 more git spawns on a cache miss. Process spawns cost 50–200 ms on Windows, which caps the writer at a few dozen events per second. Pre- and PostToolUse fire on every tool call. The stall watchdog misses this because the writer is slow but still moving. | `writer.rs:673`, `project.rs:105-131,645`, `supervisor.rs:40` |
| F7 | Writer blocked by token sweep | Every Stop or SubagentStop re-reads all sibling `agent-*.jsonl` files inline. Offsets are keyed per session, so old sidecars are re-read from 0, one SQL statement per turn, with no transaction. | `writer.rs:862-904,938-1120` |
| F8 | No spool | Any shim failure (daemon down, busy pipe after 1 s, timeout) drops the event and only writes a breadcrumb. Nothing is replayed on start. | `main.rs:1667-1693,1765` |
| F9 | Hooks dropped at daemon start | The pipe binds at `lib.rs:390` but nothing accepts until `:479`, after the store migration and state build. | `dirad/src/lib.rs` |
| F10 | Events skipped by sync | Ids are non-monotonic ULIDs minted before the event is queued. Once the event cursor has passed an id, that event never syncs. | `events.rs:17`, `lib.rs:736` |
| F11 | Sync wedged forever | A 413 or any other 4xx retries the same chunk forever, so one poisoned row stops the event cursor. | `sync.rs:1114-1154` |
| F12 | Wrong advice | Sync records `unknown_device`, but doctor matches `device_unknown`. | `sync.rs:221`, `doctor/checks.rs:696` |
| F13 | Onboarding friction | About 2 commands, 6 prompts and 3–5 follow-ups. Codex is paste-by-hand. Installers only print "run dira onboard". The command surface overlaps: 4 ways to wire hooks, 5 to start the daemon, 6 status-like commands. | `onboard/*`, `init.rs:860-905`, `main.rs` |

Privacy note for the spool: `Request::IngestHook` currently ships the **raw** harness payload
(prompts, tool input) over IPC. A spool must never write that to disk, so the shim must normalize
first.

## Phase 0: Reproduce on Windows (needs your go-ahead; nothing is installed yet)

**Before starting, the user needs to:**
- open Parallels Desktop (`prlctl` can't initialize until the app runs);
- start or log in to the **Windows 11** VM.

**Steps:**
- I drive the VM with `prlctl exec --current-user "Windows 11" …`, which runs as the logged-in
  **standard** user (never SYSTEM), plus `prlctl capture` screenshots for console windows.
- Baseline: install the current release (`v0.12.0-develop.2`) with `install.ps1` and record F1–F5:
  - console windows after install, `dira update` and logon;
  - whether `schtasks /Create` succeeds or the Run key is used;
  - the daemon state after reboot.
- Hook-flood script (PowerShell): run 2,000 concurrent `dira hook claude` payloads, then compare
  events stored against events sent, during steady state, with dirad stopped, and during dirad
  startup.
- Fixed builds: `cargo xwin build --release --target x86_64-pc-windows-msvc`. You accept the
  Microsoft CRT/SDK license on first run. The VM reads the binaries through the Parallels shared
  folder, and they run under x64 emulation. Final arm64 validation comes from the PR's CI Windows
  lanes.
- Builds and tests run one at a time. Every Mac-side daemon test sets `DIRA_SOCKET_PATH`,
  `DIRA_DB_PATH` and `DIRA_HTTP_PORT`, so the real `dirad` is never touched. UDS tests run without
  the sandbox.

## Phase 1: Windows daemon is windowless and admin-free (fixes F1–F4)

**Windowless dirad**
- `#![cfg_attr(windows, windows_subsystem = "windows")]` on `cli/dirad/src/main.rs`.
- `dirad` takes no interactive flags. If one prints, route it to a log or `AttachConsole`.
- Add `CREATE_NO_WINDOW` to every spawn dirad makes: `zavet_specs.rs:291` `SystemSpecRunner`,
  the git spawns, and the capture.
- Add one shared `dira_core::process::hidden(Command)` helper and a test that greps for bare
  `Command::new` in the dirad crate.

**Logon task from XML** (`schtasks /Create /XML` with a per-user temp file, written by a new
`daemon/windows_task.rs`):
- `LogonTrigger` with `UserId` = the current user (no admin needed).
- `InteractiveToken`, `LeastPrivilege`.
- `DisallowStartIfOnBatteries=false`, `StopIfGoingOnBatteries=false`.
- `ExecutionTimeLimit=PT0S`, `RestartOnFailure` every 1 min (count 999).
- `StartWhenAvailable`, `Hidden`, `MultipleInstancesPolicy=IgnoreNew`.

**Run-key fallback is real supervision**
- New `Supervision::RunKey` variant.
- Install **starts dirad now** with a hidden detached spawn.
- Restart is Shutdown → confirmed exit (D-0019) → hidden spawn.
- Uninstall stops the process and deletes the key.

**Supervised daemons are stoppable**
- dirad writes its own pidfile after bind (on all OSes), so `stop`, `restart` and `uninstall`
  always resolve a pid.
- On Windows, `daemon stop` is Shutdown → `tasklist` wait → `taskkill`, never only `/End`.

**Self-healing start (all OSes)**
- When the shim gets `Down`, it spawns `dirad` itself: hidden, detached, and rate-limited by a
  lock file to once per 30 s.
- It does this only when the shim's token is **not elevated** (D-0016: an elevated daemon is the
  broken case) and not in a dev build. D-0009's bind guard makes racing spawns safe.

**macOS/Linux supervision**
- macOS: `launchctl bootstrap/bootout gui/$UID` + `kickstart -k` instead of the deprecated
  `load`/`unload`.
- Linux: keep `systemctl --user`, and add a doctor hint about `loginctl enable-linger`.

**Decision record:** DIRASH-0048, "The Windows daemon is a windows-subsystem binary supervised by
an XML logon task or a Run key, both started immediately."

## Phase 2: Zero event loss (fixes F5–F11)

**Durable spool**
- Location: `<data_dir>/spool/`. One file per event, `<ulid>.json`, written tmp + rename. No
  locks, safe for concurrent hooks.
- It holds the **normalized** event only, never raw payload text, plus `observed_at` taken in the
  shim.
- The shim spools on `Down`, `Busy` or timeout, then exits 0 as before. The breadcrumb stays.
- Bounds: 100k files or 64 MB, dropping the oldest, with a counter in `meta`.

**IPC v2**
- New `Request::IngestEvent { harness, event: NormalizedHook, observed_at }`.
- The shim normalizes using the `dira_sources` mappers it already links. Raw content stops crossing
  the pipe.
- `IngestHook` stays accepted for older shims during upgrade skew.

**Daemon never drops**
- If the queue is full, the daemon **spills the message to the spool** and returns Ok. Counters
  record it.
- A drain task replays the spool at startup, every 30 s, and whenever the queue empties, keeping
  the original `observed_at`.

**Accept immediately**
- Start the accept loop right after bind (F9). Ingest goes into the channel, or spills, while the
  store opens. Control requests that need the store get `Busy` until it is ready.

**Faster writer**
- Read the branch from `.git/HEAD` directly (handling the worktree `gitdir:` pointer) instead of
  spawning `git` (F6).
- Cache project resolution per cwd with a TTL. Move the remaining git calls to `spawn_blocking`.
- Batch inserts: drain up to 256 messages, then write them in one transaction.
- The token sweep becomes its own task with its own queue. Sidecar offsets are keyed by the
  sidecar file, not the session, so old sidecars are never re-read (F7).
- The stall watchdog also fires on "queue depth > 50 % for 60 s".

**Monotonic cursor**
- The event sync cursor moves to `rowid`, like D-0018, and the ULID generator becomes
  `ulid::Generator` (monotonic) (F10).
- Migration `0010` converts the stored cursor once.

**Poison quarantine**
- On a 413, halve the chunk size.
- On another non-auth 4xx, bisect down to the offending rows, move them to `sync_quarantine` with
  the reason, and advance the cursor (F11).
- `dira doctor` reports how many rows are quarantined, and `dira device resync --quarantined`
  retries them.

**Visible health**
- `capture_health` counters: spooled, spilled, dropped, drained, quarantined. `dira status` shows
  one line, `dira doctor` checks them, and they feed contract 2.0.
- Fix F12.

**Decision records**
- DIRASH-0049, "A hook event is either stored or spooled, never dropped; the spool holds
  normalized metadata only."
- DIRASH-0050, "The event cursor rides rowid."

## Phase 3: Silent background auto-update (supersedes part of D-0006)

**Scheduler**
- dirad runs a timer every 6 h ± 30 min of jitter, plus once 10 min after start. It spawns a
  hidden, detached `dira update --auto`.
- The daemon only schedules. All network and swap logic stays in the CLI.
- The D-0006 foreground rule is unchanged: no network on `status`, `version`, `hook` and so on.

**`--auto` flow:** this reuses `update::run` unchanged:
- resolve the release on the pinned channel;
- `discover_install` (refuses dev installs, D-0004);
- download, check sha256, extract;
- `replace::swap_binaries` (D-0003);
- probe `dira --version`;
- restart dirad: Shutdown → confirmed exit → supervisor start (D-0019);
- DIRASH-0038 cloud refresh in the success arm.

What `--auto` adds:
- a lock file;
- writes `update-state.json` (last attempt, result, from/to);
- quiet output to `logs/update.log`;
- no prompts;
- skips the update when a session is active right now, retrying at the next tick;
- backs off after a failure using the shared `Backoff`.

**Settings and commands**
- **Opt-out:** `update.auto=false`, `DIRA_DISABLE_AUTOUPDATE=1`, and dev builds. `CI=true` also
  disables it.
- **Channel pin:** `update.channel = stable|prerelease`, defaulting to the channel of the
  installed version. It is written by the installers, and `dira update --channel` persists it.
- **Rollback:** after a successful swap, the previous binaries are kept as
  `.dira.prev`/`.dirad.prev`. `dira update --rollback` swaps back through the same rename path and
  pins `update.skip_version` so auto-update won't reinstall the bad version.
- The update notice stays for people who opt out.

**Decision record:** DIRASH-0051, "Updates apply in the background by default; dirad only
schedules, the CLI performs." It supersedes D-0006's rejected alternative "have dirad check on its
heartbeat", with the reason: the daemon is the only always-on process, and the user wants Claude
Code-like transparency.

## Phase 4: One-command install and onboarding, smaller surface

**Installers** (`install.sh`, `install.ps1`)
- Register the per-user service **by default**, terminal or not. `--no-service` opts out. This
  supersedes the onboarding spec's "never unattended" rule and amends DIRASH-0029.
- Write `update.channel`.
- With a tty, `exec dira onboard` at the end.
- Without a tty, print one line telling the user to run `dira onboard`.

**Admin shells**
- `install.ps1` in an Administrator shell still refuses to start dirad.
- It now also registers the task with an explicit standard-user principal and prints a single
  clear note.

**Onboard**
- One combined prompt: "Wire Claude Code, Codex, Cursor? [Y/n]".
- **Codex gets wired automatically** with `toml_edit`, which preserves the user's comments and
  formatting.
- The device link stays optional.
- Re-running `dira onboard` inside any repo does only the missing repo steps.
- Fix the step-count drift between `--help`, the spec and `getting-started.md`.

**Command surface** (as hidden aliases, so no scripts break)
- `daemon start` ensures supervision (it absorbs `daemon install`). `daemon install` becomes a
  hidden alias.
- `init` becomes a hidden alias that points at `onboard --harness`.
- `status` gains a one-line health and capture summary, so `daemon status` and `device status`
  stay only as detail views.

**Docs**
- `docs/install.md`: Windows column, auto-update, and opt-out.
- `docs/getting-started.md`: shorter flow.

## Phase 5: Contract 2.0 + local store (CLI side; the cloud redesign session mirrors it)

`SCHEMA_VERSION = "2.0.0"`. Regenerate the schema and signing vector with `just contract`. The
CLI emits only 2.0. Rollout order (from the merge-order memory):
1. CLI-develop
2. cloud-develop
3. cloud-main, accepting 1.x + 2.0
4. CLI-main

CLI-main is released only after cloud-main serves 2.0, so there is no dual emitter.

**Envelope / batch (breaking)**
- `AttestationBatch.seq: u64`: monotonic per device and chain epoch.
- `prevBatchHash: String`: sha256 hex of the JCS payload of the **last acked** batch.
  - It is `""` at `seq=0`.
  - A retry resends the identical payload and `batch_id`, and the cloud dedups it.
  - `chainEpoch: u32` resets on `nuke`, re-link or key rotation.
- `client: ClientInfo { version, channel, os, arch }`.
- `captureHealth: CaptureHealth { spooled, spilled, dropped, drained, quarantined, hookFailures }`:
  deltas since the previous batch.
- Transport: `Content-Encoding: zstd` on the body. The signature still covers the uncompressed
  JCS payload.
  - The cloud advertises `acceptEncodings` in `ServerMeta`; without it the CLI sends identity.
  - Chunks grow to events 5000, artifacts 500, tokens 5000, with a 3 MB compressed cap (below
    Vercel's 4.5 MB body limit).

**`SessionRollup.stats: SessionStats`** (counts only, per D-0001)
- `toolCalls: { read, edit, write, search, shell, web, mcp, task, other }`. The category comes
  from the tool name in the shim. MCP **server names are not sent**.
- `filesTouched: u32`: distinct paths, counted locally.
- `agentLinesAdded`, `agentLinesRemoved: u32`: counted in the shim from the Edit/Write/MultiEdit
  inputs, which are then discarded.
- `subagents`, `compactions` (PreCompact), `permissionPrompts: u32`.
- `approvalWaitSeconds: u64`: from a PermissionRequest/Notification to the next tool or prompt
  event.
- `testRuns: { passed, failed }`, **best-effort**. The shim classifies the Bash command prefix
  (cargo/npm/pnpm/bun/pytest/go/vitest/jest test) and the exit status of the PostToolUse response,
  then sends counts only.
- **Pushback, model mix:** dropped. `token_usage[].model` already carries it per turn, so the
  cloud derives it.
- **Pushback, interrupts:** dropped from 2.0. No hook signals them; only transcript text scraping
  could, which is fragile. Revisit in 2.x.
- **Pushback, human vs agent lines:** `ArtifactRef` gains `linesAdded`/`linesRemoved` from git
  numstat per commit. The cloud derives "human ≈ commit − agent" and labels it an estimate. A
  "human lines" field would claim more precision than the data has.

**`dayRollups: Vec<DayRollup>`** (new, signed in the same batch)
- Fields:
  - `day` (UTC date), `repoCanonical: Option`, `workspaceHint: Option`;
  - `humanSeconds`, `agentWallSeconds`, `sessions`, `prompts`, `commits`;
  - `tokens: Vec<{model, input, output, cacheRead, cacheCreate}>`, `estCostUsd`;
  - `intervalCount`, `intervalDigest` (sha256 of the sorted interval ids);
  - `revision: u32`.
- A day's rollup is resent with `revision+1` whenever late data changes it. The cloud compares it
  with `facts_daily` and flags drift.
- **Pushback:** it uses the UTC day. The cloud re-buckets to the workspace timezone from
  intervals, so the rollup is a reconciliation check, not the display source.

**Routing**
- `SessionRollup.workspaceHint: Option<String>` and the same field on `DayRollup`.
- The value is the cloud workspace id from the repo's `.dira` cloud pin or config.
- The cloud validates membership and ignores it if invalid. It is distinct from the existing
  lineage field `workspace_ref`.

**Meetings**
- **Pushback:** no untyped slot is reserved. Adding a typed `meetings: Vec<MeetingRollup>` later is
  allowed as a 2.x minor (additive optional field). It is defined with the meetings-plan session.

**Local store (migration `0010_v2.sql`, collapsed into one file)**
- `events`:
  - add `observed_at` (shim time) next to `at` (receipt time);
  - add `tool_category`, `lines_added`, `lines_removed`, `test_outcome`.
- New tables:
  - `sync_quarantine(rowid_ref, stream, reason, at)`;
  - `capture_health` counters;
  - `day_rollups(day, repo, workspace, digest, revision, sent_hash)`.
- `meta` keys:
  - `event_cursor_rowid` (replaces `event_cursor`);
  - `batch_seq`, `batch_prev_hash`, `chain_epoch`.
- Stats are computed at batch time from `events`. There is no per-session stats table.

**Decision record:** DIRASH-0052, "Contract 2.0: chained, compressed batches with day rollups and
count-only session stats."

## Specs to update in the same commits (with `Spec:` trailers)

- `distribution-and-update`: auto-apply, channel pin, rollback, Windows task XML.
- `daemon-lifecycle`: windowless, self-pidfile, accept-before-store, self-healing start.
- `harness-sources` / `capture-pipeline`: spool, IPC v2, spill, writer batching.
- `attestation-sync`: rowid cursor, quarantine, 2.0 chain and rollups.
- `onboarding`: service by default, Codex auto-wire, installer handoff.
- `doctor`: new checks.

## Delivery

- **Branch:** `fix/cli-transparent-install-update` off `develop`. Conventional commits with scope
  `cli`/`daemon`/`contract`, each `-s` signed.
- **PRs:**
  - PR A: Phases 1–4.
  - PR B: Phase 5. It is stacked on A and merges after the cloud 2.0 PR, per the gate memory.
  - Each opened with `gh` only after you OK it.
- **Follow-up issues** (`gh issue create`):
  - release signing (minisign over `checksums.txt`, key in a GH secret + 1Password);
  - browser device-code link flow (cloud);
  - an MSIX/winget package.

## Verification

- **Unit/integration** (`just ci`, run one at a time, UDS tests without the sandbox):
  - the spool round-trip;
  - the full-queue spill, using a capacity-1 test build;
  - accept-before-store, with a slow-store fake;
  - rowid cursor with out-of-order inserts;
  - 4xx bisection/quarantine against a mock server;
  - the Windows task XML golden file;
  - the `Supervision::RunKey` paths through `SystemRunner` fakes;
  - auto-update with a fake release server, plus `--rollback`;
  - the contract drift gate and the signing vector.
- **New e2e test `hook_flood`** (D-0021 isolation, also on the Windows CI lane):
  - 2,000 concurrent `dira hook` calls with dirad down, then started → all 2,000 stored, spool
    empty;
  - the same during startup and during steady state.
- **Windows VM** (Phase 0 scenarios, re-run on fixed builds):
  - no console window after install, update or reboot (screenshots);
  - the daemon runs after reboot as a standard user, through both the XML task and the forced
    Run-key path;
  - `dira update` from the baseline leaves dirad running;
  - the hook flood loses 0 events;
  - an unplugged-battery simulation (task settings checked via `schtasks /Query /XML`).
- **macOS:** an isolated daemon (`DIRA_SOCKET_PATH`/`DIRA_DB_PATH`/`DIRA_HTTP_PORT`) runs the flood
  and auto-update against a local fake release. The real `dirad` is untouched.
- **Release config:** `bunx semantic-release --dry-run --no-ci --extends ./.releaserc.js` (after
  `bun install`).
