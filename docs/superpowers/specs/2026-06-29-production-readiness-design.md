# Dira CLI — production-readiness design

**Date:** 2026-06-29
**Branch:** `feat/real-anchoring-author-session` (all work lands as a sequence of focused commits on this branch)
**Status:** approved roadmap; PR-1 (stability) speced in detail below, later commits speced at the design level and refined just-in-time.

## Goal

Take the `dira` / `dirad` binaries to production quality across seven fronts the
user named: more tests, in-place simplification, squash-resilient anchoring,
daemon/CLI stability (stuck timers), faster `dira status` at session start, a
`dira device unlink` command, and a `dira version` command that also reports the
running daemon's version.

Each numbered section below is one focused commit. They are ordered worst-pain-first
and land sequentially on the current branch. Integration tests and targeted
simplification ride along with the commit that touches the relevant code rather than
being deferred to a separate pass.

## Decisions locked in brainstorming

- **Packaging:** decompose into focused *commits* on the current branch, in the order below.
- **Anchoring scope:** this repo emits richer anchor *signals* on the wire and documents
  the intended cloud matching; the re-anchoring algorithm itself lives in the
  proprietary cloud repo and is out of scope here.
- **Unlink semantics:** local unlink — clear `device_id`, **keep** the keychain key,
  warn on unsynced backlog; no cloud revoke call (no endpoint exists in this repo).
- **Test depth:** add the missing integration tests (daemon↔CLI socket, sync round-trip,
  device link/unlink, timer-stall regression); keep adding unit tests opportunistically.

---

## Commit 1 — Daemon/CLI stability (stuck timers)

### Problem

The single writer task in `cli/dirad/src/main.rs` drains the ingest queue and, on
commit-bearing events, calls `capture_commits()` **inline**. `capture_commits` shells
out to `git` (`rev-parse`, `log`, `diff-tree | patch-id`) synchronously with no
timeout. If git blocks (index.lock contention, slow filesystem, a huge repo, a hung
credential helper), the writer stops draining — so **every** session's `active_seconds`
stops accruing and manual `ManualTick`s queue without being processed. The dashboard's
client-side interpolation (commit `b5ca744`) masks this only until the idle window
elapses, after which timers visibly freeze.

Secondary fragility:
- `control.rs` reads the sessions registry with `state.sessions.lock().unwrap()` at
  three sites (status/sessions/stop). A panic anywhere a writer holds the lock poisons
  it, and these `.unwrap()`s then crash the CLI-facing control handler.
- No watchdog: if the writer or idle ticker stalls or dies, nothing notices or recovers.

### Approach

1. **Get git off the hot path.** Move `capture_commits` work to `tokio::task::spawn_blocking`
   (git is blocking IO) wrapped in a `tokio::time::timeout`. The writer enqueues a
   capture request and continues draining immediately; capture results are applied when
   the blocking task returns. A timed-out capture is logged and dropped — it retries on
   the next commit-bearing event. The writer never blocks on git again.
2. **Remove panic paths.** Replace the three `.lock().unwrap()` sites with a small
   poison-tolerant helper (`lock_recover`) that takes the guard even when poisoned
   (`PoisonError::into_inner`). A poisoned registry degrades to stale-but-serving rather
   than crashing the control surface.
3. **Watchdog.** Add a supervisor that tracks a "last writer progress" timestamp
   (updated each drained message) and the idle ticker's last tick. If either exceeds a
   threshold, log a warning with diagnostics. Tasks are spawned with explicit restart on
   panic (re-spawn the writer/ticker if their `JoinHandle` resolves to an error) so a
   one-off panic self-heals instead of silently ending accrual.

### Tests

- **Regression (stall):** a `capture` hook that blocks past the timeout must not prevent
  subsequent `ManualTick` events from advancing `active_seconds` — drive the writer with
  a fake slow-git and assert the registry keeps incrementing.
- **Poison tolerance:** poison the sessions mutex, then assert `status`/`sessions` still
  return a response instead of panicking.
- **Watchdog:** simulate a stalled writer and assert the supervisor logs/recovers.

### Simplification carried along

`dirad/main.rs` (754 lines) mixes daemon bootstrap, the writer loop, git capture
throttling, and the idle ticker. Extract the writer loop and the capture/throttle logic
into focused modules (e.g. `writer.rs`, `capture.rs`) so `main.rs` is just wiring. This
is the file we're already editing, so the split is in-scope.

---

## Commit 2 — `dira status` reactivity at session start

### Problem

In `cli/dirad/src/main.rs` the startup sequence loads config → opens store → loads the
device key (can block on a keychain unlock prompt) → **`hydrate()` replays ~1 day of
events** → only *then* binds the HTTP and UDS control sockets. `dira status` cannot
connect until the socket is bound, so the perceived latency right after a session starts
is daemon-not-ready, not the (fast, SQL-only) status query.

### Approach

- **Bind the control socket before hydration.** Reorder startup so the UDS (and `/healthz`)
  bind first and the daemon answers `Ping`/`Status` immediately, then run `hydrate()` on a
  background task that populates the registry. A status issued during hydration returns a
  valid (initially sparse) view that fills in within a frame or two — far better than a
  connection error or a multi-second hang.
- **Keychain off the critical path.** The device key is only needed for sync/signing, not
  to answer control requests. Load it lazily/in the background so a keychain prompt never
  delays socket readiness.
- Optionally expose a `hydrating: bool` (or `ready` flag) in the status response so the CLI
  can show "warming up" rather than implying zero activity.

### Tests

- Integration: start the daemon, immediately `Ping` and `Status`, assert both succeed
  before hydration completes (inject an artificially slow hydrate).

---

## Commit 3 — `dira version` (CLI + daemon)

### Problem

`dira --version` works via clap, but there is no `dira version` subcommand and **no way
to see the running daemon's version** — important when the CLI and a long-lived daemon
drift across upgrades. No protocol or HTTP endpoint returns daemon build info.

### Approach

- Add `Request::DaemonInfo` / `Response::DaemonInfo { version, schema_version, pid, uptime_seconds }`
  to `cli/core/src/protocol.rs`, dispatched in `control.rs`.
- Add a `dira version` subcommand that prints the CLI version (`CARGO_PKG_VERSION`) and the
  contract `SCHEMA_VERSION`, then best-effort queries the daemon and prints its version,
  schema version, pid, and uptime (or "daemon not running"). Flags a version skew between
  CLI and daemon.
- Add `--version` to `dirad` (clap `version`).
- Optionally embed the git short hash + build date via a small `build.rs` using
  `CARGO_PKG_VERSION` + env, surfaced in both binaries. (Kept optional to avoid build
  non-determinism concerns; decide at implementation time.)

### Tests

- Unit: `version` formatting with/without a reachable daemon; skew detection.
- Integration: `DaemonInfo` round-trips over the control socket.

---

## Commit 4 — `dira device unlink`

### Problem

There is `device link`, `device status`, and `device rotate-key`, but no `unlink`. The
only documented path is "clear the device_id in the store and run link again," which is
manual and undocumented to users.

### Approach

Mirror the existing `device` subcommand shape in `cli/dira/src/device.rs`:

- `dira device unlink [--yes]`:
  1. Read `device_id`; if absent, report "not linked" and exit 0.
  2. Count unsynced events (reuse the `device status` backlog logic); if > 0, warn and
     require confirmation unless `--yes`.
  3. Clear `META_DEVICE_ID` (add `identity::clear_device_id(&store)` alongside the existing
     `set_device_id`). The daemon re-reads device linkage on its next sync tick and stops
     syncing — no restart required, matching how `link` takes effect live.
  4. **Keep** the keychain/meta secret and pubkey so a later `link` reuses the same device
     identity. Print a note that the key is retained and that re-linking will reclaim it.
  5. No cloud call — there is no revoke endpoint in this repo. Document that cloud-side
     rev, if wanted, is a separate cloud-repo change.

### Tests

- Unit: `clear_device_id` makes `is_linked` false while leaving the key/pubkey intact;
  re-`set_device_id` re-links with the same key.
- Behavior: unlink with backlog requires `--yes`; unlink when not linked is a no-op.

---

## Commit 5 — Squash-resilient anchoring signals

### Problem

Anchoring today is per-commit: each `ArtifactRef` ships `sha`, `authoredAt`,
`authorEmail`, `sourceSession`, `branch`, and `changeId` (= `git patch-id --stable`).
The patch-id survives **rebase, amend, and cherry-pick** of an individual commit because
those preserve the per-commit diff. A **squash merge** does not: it collapses N commits
into one new commit whose combined diff has a patch-id matching none of the originals.
Squash-on-merge is the default on many remotes, so the user is correct that anchoring
degrades there. The cloud can still fall back to session boundaries + branch + author +
time window, but those are weak signals.

### Key insight

A squash merge's resulting commit has a diff equal to the **cumulative** diff of the
squashed branch (`base..tip`), and its tree preserves the **post-image blob SHAs** of the
files the branch touched (absent conflict resolution). So if the device ships, per
session, signals computed over the *cumulative* change rather than per individual commit,
the cloud can re-anchor a squashed commit:

1. **`sessionChangeId`** — `git patch-id --stable` over the cumulative diff
   `merge-base(upstreamBase, HEAD)..HEAD`. Equals the squashed commit's patch-id when the
   base hasn't moved. Strong exact-match signal.
2. **`touchedPaths`** — the set of repo-relative file paths the session changed. Survives
   squash and rebase (the squashed commit touches the union of paths). Enables fuzzy
   matching (path-set overlap) when `sessionChangeId` misses due to base movement or
   conflict resolution.
3. **`blobs`** — per touched path, the git post-image blob SHA (the object id git already
   stores). A squashed commit's tree carries the same blob SHA for any file not further
   modified during the squash, so blob-set overlap is a robust content-identity anchor that
   the cloud can verify against the remote (which exposes the same blob SHAs).

These are layered: exact `sessionChangeId` match first, then blob-set / path-set overlap
as graceful degradation. All three are **metadata** (hashes and paths) — no diff or file
content crosses the boundary.

### Constraints

- **Privacy denylist (CI-gated):** `contract/src/lib.rs` rejects any wire field whose
  snake/camel tokens include `prompt|content|diff|body|text|patch|code|snippet`. New field
  names must avoid these — hence `sessionChangeId` (not `patchId`/`diffId`), `touchedPaths`,
  `blobs` (git's term, no forbidden token). Values are hashes/paths only.
- **Cost:** computing a cumulative patch-id and listing tree blobs is one extra `git`
  invocation per session capture. It runs through the same `spawn_blocking` + timeout path
  introduced in Commit 1, so it cannot stall the writer.
- **Nullable / best-effort:** like the existing `changeId`, all new signals are optional —
  `None` on merge commits, detached HEAD, missing upstream base, or git failure.

### Wire / schema changes

- Extend the session rollup (or `ArtifactRef`, decided at implementation) with optional
  `sessionChangeId: Option<String>`, `touchedPaths: Option<Vec<String>>`, and
  `blobs: Option<Vec<{ path: String, blob: String }>>`.
- Regenerate `contract/attestation.schema.json` and `contract/testdata/signing-vector.json`
  via the **`contract-sync` skill** (`just contract`) and confirm no drift — this is the
  producer repo; the cloud vendors these artifacts.
- The signed batch already covers all payload fields, so the new signals are attested
  automatically.

### Cloud algorithm (documented here, implemented in cloud repo)

To re-anchor a session whose commits were squashed/rewritten out of the remote:
1. Find candidate commits on the session's `branch` by `authorEmail` within the session's
   `[startedAt, endedAt]` window (± slack).
2. Confirm by, in priority order: (a) `sessionChangeId == candidate patch-id`; else
   (b) Jaccard overlap of `blobs` vs the candidate's changed-file blob SHAs above a
   threshold; else (c) `touchedPaths` overlap as a last resort.
3. Anchor the session's intervals to the matched commit.

### Tests

- Unit (`project.rs`): cumulative `sessionChangeId` is stable across a simulated rebase of
  the same logical change; equals the patch-id of an equivalent squashed commit; `None` on
  the documented edge cases.
- Unit: `touchedPaths` / `blobs` reflect the cumulative change, not just the last commit.
- Contract: schema + signing-fixture drift check passes (`just contract`).

---

## Cross-cutting: integration tests

New `tests/` integration suites (none exist today) added with the commits that exercise
them:
- **Daemon↔CLI socket protocol:** spin up a daemon against a temp store + socket, drive
  `Ping`/`Status`/`Sessions`/`DaemonInfo`/`Start`/`Stop` and assert framed round-trips.
- **Sync/batch round-trip:** events → `build_batch` → signed envelope → schema-valid,
  signature verifies, cursors advance.
- **Device link/unlink:** link against a stub HTTP cloud, assert `device_id` persisted;
  unlink clears it and keeps the key.

## Out of scope

- Cloud-repo changes (re-anchoring algorithm, a device revoke endpoint).
- A standalone large-refactor pass — simplification is in-place, scoped to files each
  commit already touches.
- Bumping versions by hand — releases are semantic-release driven.

## Verification

`just ci` (fmt + clippy + tests + contract schema + signing fixture) green after each
commit; `just contract` clean after Commit 5; manual dogfood of the stability and status
fixes via `just dogfood`.
