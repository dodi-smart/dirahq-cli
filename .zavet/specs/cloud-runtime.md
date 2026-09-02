---
title: Cloud agent runtimes (teleport)
version: 2
origin: session
verified: false
confidence: high
date: 2026-09-02
paths:
  - cli/dira/src/cloud_init.rs
  - cli/dira/templates/
  - cli/dira/src/hook_yield.rs
  - cli/core/src/runtime.rs
  - cli/core/src/httpclient.rs
  - cli/dira/src/device.rs
decisions: [D-0001, D-0009, D-0011, DIRASH-0033, DIRASH-0037]
checks:
  - generated scripts substituted + POSIX + contracts :: cargo test -p dira --bin dira -- cloud_init::tests::generated_scripts_are_substituted_and_shell_sane
  - portable commands read as wired by the shared matcher :: cargo test -p dira --bin dira -- init::reader_tests::portable_wrapper_commands_read_as_wired
  - injection replaces absolute-path entries, converges :: cargo test -p dira --bin dira -- cloud_init::tests::nested_injection_replaces_absolute_path_entries_and_is_idempotent
  - end-to-end artifacts + idempotency + dry run + git-toplevel refusal + malformed-config refusal + gitignore warning :: cargo test -p dira --test cloud_init_e2e
  - digest pinning: no-pin skips the fetch, a good pin survives a failed re-fetch, a pin is never reused for a different version :: cargo test -p dira --bin dira -- cloud_init::tests::resolve_digests_no_pin_never_fetches_and_is_empty cloud_init::tests::resolve_digests_keeps_a_good_pin_on_a_failed_refetch cloud_init::tests::resolve_digests_does_not_reuse_a_pin_for_a_different_version cloud_init::tests::fetch_release_digests_embeds_both_targets_from_a_scripted_server
  - dira init over a cloud-wired project adds nothing at project scope, --global still writes :: cargo test -p dira --test cloud_init_e2e -- dira_init_over_a_cloud_init_repo_adds_nothing_and_global_still_writes
  - merge mode treats the portable wrapper as already wired, replace mode still matches exactly :: cargo test -p dira --bin dira -- init::reader_tests::merge_mode_treats_a_portable_wrapper_as_already_wired init::reader_tests::replace_mode_does_not_treat_an_arbitrary_wrapper_spelling_as_current
  - the portable hook yields to a live catch-all user-scope entry, never to a matcher-scoped or portable one; the marker alone never yields without a genuine project-scope portable wrapper too :: cargo test -p dira --bin dira -- hook_yield::
  - a real portable invocation yields, a direct one still forwards, a stray marker with no project-scope portable wiring still forwards, against real config files :: cargo test -p dira --test hook_yield_e2e
  - runner claim without a TTY, the runner token wins over a code, a retried claim reuses its nonce and clears it only on success :: cargo test -p dira --bin dira -- device::tests::runner_token_link_claims_without_a_tty device::tests::link_prefers_the_runner_token_over_a_code_when_both_are_given device::tests::a_retried_claim_reuses_the_same_client_nonce_and_clears_it_on_success
  - runtime detection is conservative, an explicit override never carries a stray session ref, both fields clamp to 64 chars :: cargo test -p dira-core --lib -- runtime::
  - extra CA roots never brick the client, a bundle with one corrupt block still loads the valid certs alongside it :: cargo test -p dira-core --lib -- httpclient::
  - DIRA_IDENTITY_EMAIL needs an @ and a plausible length, else falls through to git config :: cargo test -p dira-core --lib -- project::tests::env_identity_email
  - sync cadence knobs clamp and default to the historical constants, DIRA_SYNC_BACKSTOP_SECS reaches sync_backstop() :: cargo test -p dira-core --lib -- config::tests::sync_cadence config::tests::env_sync_backstop_secs_reaches_sync_backstop
  - cloud doctor judges :: cargo test -p dira --bin dira -- doctor::checks::tests::a_cloud
  - a real agent session in a simulated cloud runtime is captured :: sh .github/scripts/cloud-capture-smoke.sh
  - 1.4 runtime fields round-trip and stay off pre-1.4 wires :: cargo test -p dira-contract -- session_runtime_roundtrips
  - the daemon stamps rollups only inside a detected cloud runtime :: cargo test -p dirad --lib -- rollups_carry_the_cloud_runtime
  - bootstrap template is syntactically valid POSIX sh and shellcheck-clean :: sh -n cli/dira/templates/dira-bootstrap.sh && shellcheck -s sh cli/dira/templates/dira-bootstrap.sh
---

## Overview

How dira captures agent work in cloud runtimes — Claude Code on the web and
Cursor cloud agents — where each session runs in a fresh ephemeral VM, the
repository is the only reliable delivery channel, all egress passes a
TLS-intercepting proxy, and nobody is at a keyboard to link a device.
`dira cloud init` generates repo-committed portable artifacts; a SessionStart
bootstrap installs the pinned release, starts the daemon, and claims an
ephemeral device with a runner token; sync flushes eagerly because the VM can
be reclaimed without warning. A second, narrower problem sits alongside the
cloud story and shares its machinery: an engineer who has *also* onboarded
their laptop (`dira init --global`) gets the same event delivered twice when
they open a teleport-ready repo there — the portable wrapper yields to that
live wiring instead (DIRASH-0037).

## Behavior

- `dira cloud init` writes `.dira/hook.sh`, `.dira/bootstrap.sh` (version- and,
  unless `--no-pin`, digest-pinned from the generating binary), `.dira/
  .gitattributes` (`*.sh text eol=lf`, so a Windows checkout doesn't hand the
  bootstrap CRLF line endings), and merges portable commands into the project
  `.claude/settings.json` and `.cursor/hooks.json`. Machine-specific
  `dira init` entries for the same harness are REPLACED, not duplicated —
  a repo never carries both a broken and a portable form of the same hook.
  Non-dira hook entries are never touched. Re-running is a fixpoint. Every
  write resolves the git work tree's toplevel first and writes there, never at
  `cwd`, so the command works the same run from any subdirectory; a
  non-`--print` invocation outside a git work tree refuses with an actionable
  message naming `--print` and `cd`. `--print` always proceeds from `cwd`
  unconditionally — it needs no repository at all.
  When wiring more than one harness at once (the bare, all-harnesses form, or
  an explicit `--harness a,b`), an unparseable existing hook config is a hard
  refusal rather than a silent overwrite; the single-`--harness` form still
  overwrites, on the reasoning that a caller naming exactly one harness has
  already made the call.
  `dira init` run over an already `cloud init`-wired project adds nothing at
  project scope — it recognizes the portable wrapper as "already wired" and
  prints an explanatory note instead of laying a redundant machine-specific
  entry beside it — while `dira init --global` still writes its own entry, at
  user scope, exactly as before; that is what the portable-vs-user-scope yield
  below exists to reconcile at runtime. `dira cloud init`'s own replace mode
  keeps exact command-string matching (no such recognition), so a stale
  wrapper spelling from an older `dira` is still replaced rather than left in
  place.
  Best-effort, `!print_only`-only warnings (never fail the command): a dev or
  gitignored-workflow build (`update::replace::discover_install`'s own
  `Guard::DevBuild`/`Guard::DevSymlink` predicate — the same one `dira update`
  itself uses) or a prerelease version pin; each written harness config that
  `git check-ignore` reports as ignored (names the path and the fix); and, on
  Windows, that the committed command needs Git Bash's `sh` to run at all.
- `.dira/hook.sh` carries the hook shim's contract: always exit 0, nothing on
  stdout, resolve `dira` at run time (`PATH` → `~/.local/bin` →
  `/usr/local/bin`), drop the event silently when none exists. It exports
  `DIRA_HOOK_VIA=portable` immediately before `exec`ing `dira hook <harness>`
  — the marker `hook_yield` reads (see below).
- **The portable hook yields to live user-scope wiring, DIRASH-0037 — two-sided,
  the marker alone is not proof.** `dira hook` reads `DIRA_HOOK_VIA=portable`
  and, only when it is set and the invocation is not `dira doctor --probe`'s
  own synthetic hook, requires BOTH: (1) the same event for the same harness
  is *also* wired at user (global) scope with a command that is a direct
  `dira hook <harness>` invocation (not another portable wrapper — two
  portables yielding to each other would drop the event everywhere), scoped
  by a catch-all matcher (absent/`""`/`"*"`/`".*"` — a real tool matcher like
  `"Bash"` does not cover the whole event), and resolves to
  `Exists`/`Unverifiable` (never `Missing`); AND (2) the **project**-scope
  config for this harness — `init::harness_config_paths()`'s project row,
  resolved against `CLAUDE_PROJECT_DIR` when set, else the current directory —
  genuinely wires the same event through the portable wrapper
  (`init::command_is_portable_wrapper`). Condition (2) exists because the
  marker is an ordinary env var: a `DIRA_HOOK_VIA=portable` that leaked into a
  shell profile would otherwise make a *direct* invocation yield just because
  a live user-scope entry happens to exist, silently dropping the event with
  nothing left to deliver it. If both hold, the portable invocation exits 0
  without forwarding — no daemon contact, no `hook_health` write.
  `DIRA_HOOK_DEBUG` prints one line on a yield (silent otherwise by design) —
  the only way to see one happened, and the only visibility into the residual
  gap where a launch mode drops user-scope settings entirely (Claude Code's
  `allowManagedHooksOnly` and similar): the file is still readable and
  resolvable from disk even though the harness will never run it, so the
  check can yield to wiring that never actually fires. There is no cache and
  no time window: both checks are one file read each, re-derived every
  invocation. `dira doctor`'s `hooks.scope_overlap` check judges the same
  wiring facts and reports the portable-wrapper case as `ok` (one delivery,
  working as designed) and a machine-specific double-scope entry as `warn`
  (the event genuinely fires twice), never `fail`.
- `.dira/bootstrap.sh <harness>` (the SessionStart command) provisions only
  inside a cloud runtime (`CLAUDE_CODE_REMOTE="true"`, a non-blank trimmed
  `DIRA_RUNTIME`, or `DIRA_BOOTSTRAP_FORCE=1` — both env checks trim
  surrounding whitespace before testing, mirroring `runtime::detect_from`) and
  ALWAYS ends by forwarding its own stdin event through hook.sh, cloud or not.
  Every `dira`/`dirad` subprocess `provision()` spawns reads stdin from
  `/dev/null`, so none of them can consume or block on the SessionStart
  payload waiting on this script's own stdin. Provisioning is flock-guarded
  (two racing hooks provision once; the lock lives under
  `${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}`, the same preference order D-0008 uses
  for the control socket, though this lock's `$TMPDIR` fallback carries none
  of that decision's cross-host rendezvous risk — it only ever mediates one
  VM's own racing hooks within a single boot) and every provisioning failure
  degrades to "not instrumented", never a failed hook. Two non-hook modes
  serve runtimes that provision from their own environment config:
  `--install-only` runs just the sha-verified download/install (build phase,
  cached on disk) and **exits non-zero when no `dira` binary ends up
  installed** — a build-phase failure is exactly where a runtime's own
  tooling can and should surface it; `--provision-only` installs, starts the
  daemon and claims a runner device, then exits **without** forwarding any
  event — it has no hook payload on stdin, must not fabricate one, and always
  exits 0 (hook mode and `--provision-only` both keep the always-degrade
  posture; only `--install-only` is allowed to fail loudly). Outside a cloud
  runtime, `--provision-only` prints an unconditional (not debug-gated) note
  naming the three markers it checked, then still exits 0.
  Every `curl` call — tarball and `.sha256` alike — carries
  `--proto '=https' --tlsv1.2 --connect-timeout 10 --max-time 120` alongside
  the existing `-fsSL --retry 3`.
- **Digest pinning (optional, on by default).** `dira cloud init` fetches the
  two musl `.sha256` release assets for the generating binary's own version
  (reusing `update::artifact::parse_sha256_file` and the `DIRA_DOWNLOAD_URL`
  seam so tests never touch the real network) and embeds the digests in
  `.dira/bootstrap.sh`. `--no-pin` skips the fetch outright and writes the
  unpinned form. A failed fetch never fails the command: it warns and either
  reuses an existing on-disk pin for the exact same version (so
  `write_script`'s content-diff check leaves the file untouched — never a
  downgrade to unpinned on a merely-flaky re-fetch) or falls back to the fully
  unpinned form when there is nothing to reuse. At install time the bootstrap
  prefers the embedded digest (local hash via `sha256sum` → `shasum -a 256` →
  `openssl dgst -sha256`, in that fallback order) and only fetches the
  release's own `.sha256` asset when no digest is embedded, or when
  `DIRA_VERSION` overrides the pin to a different version than the one
  embedded.
- Cursor cloud agents are provisioned from `.cursor/environment.json`
  (`install` at build, `start` on every boot), NOT from a hook: Cursor cloud
  agents do not run `sessionStart`/`sessionEnd` — Cursor's own docs list both
  among the hooks a local run supports, but name them explicitly unavailable
  to cloud agents — and every harness's hooks only start once the agent has a
  writable environment regardless, so provisioning cannot hang a build phase
  off one either way. `.cursor/hooks.json` still carries the capture wiring —
  cloud agents do run command hooks from it once the environment is
  writable — so `start` (not a hook) is what actually brings the daemon up.
- The release download comes from GitHub release assets (on cloud runtimes'
  default network allowlists) and is verified against a sha256 digest before
  anything is installed — the embedded pin when present, the release's own
  `.sha256` asset otherwise — same integrity model as `install.sh`. Linux
  targets are the musl builds. `install_dira`'s short-circuit (skip the
  download when a binary is already resolvable) checks both `dira` and
  `dirad`, not just one.
- Runtime detection (`dira_core::runtime::detect`) is conservative: Claude's
  documented marker, an explicit `DIRA_RUNTIME`, else None. It labels and
  diagnoses only — capture and accounting are identical everywhere. The
  Cursor cloud marker is unconfirmed, so Cursor environments declare
  `DIRA_RUNTIME=cursor-cloud` themselves. `DIRA_RUNTIME` (and, on the
  claude-web branch only, `CLAUDE_CODE_REMOTE_SESSION_ID`) is clamped to 64
  chars — truncated on a `char` boundary, never mid-codepoint — before it
  reaches a runner label or the wire, so a misbehaving environment variable
  can't inflate either. An explicit `DIRA_RUNTIME` override no longer carries
  a stray `CLAUDE_CODE_REMOTE_SESSION_ID` from the environment; that field is
  now read only inside the claude-web detection branch.
- Headless identity is a **runner token**: `dira device link --runner-token`
  (env `DIRA_RUNNER_TOKEN`) POSTs `{ runnerToken, ed25519Pubkey, label,
  clientNonce }` to the same `/api/v1/devices/claim` endpoint, never touches
  a TTY, and keeps every code-claim invariant — the cloud assigns the device
  id, nothing persists until it does, retries are nonce-idempotent (a pending
  claim nonce is persisted in `meta` before the POST and cleared only after a
  successful device-id write, so a retried claim after a mid-flight crash
  reuses the same nonce rather than minting a new one and orphaning the
  first). Default label: `cloud:<runtime>:<session-ref-or-hostname>`. The
  bootstrap's own `dira device link` call never puts the token on argv — it is
  already in the process environment (set by the cloud runtime, not by the
  script), and `--runner-token` is declared `env = DIRA_RUNNER_TOKEN` in
  clap, so a bare `dira device link` already picks it up. `--code` and
  `--runner-token` no longer conflict at the CLI level; when both are given,
  `device::link` prefers the runner token. `DIRA_IDENTITY_EMAIL` (attribution
  override, since a cloud VM's own `git config user.email` belongs to the
  platform bot) requires a `@` and a length under 255 chars; an implausible
  value logs a warning and falls through to git config rather than attributing
  work to garbage — the same never-brick posture as everything else here.
- TLS through the runtime's intercepting proxy is handled by
  `DIRA_EXTRA_CA_CERTS` (DIRASH-0033): additive PEM anchors appended to the
  bundled roots at every device→cloud client build site; never `SSL_CERT_FILE`,
  never a store swap, never an error on a bad bundle — a bundle with one
  corrupt PEM block still loads every valid cert alongside it rather than
  discarding the whole bundle. Measured in a live Claude Code cloud session:
  with no extra roots **every** HTTPS call fails
  `invalid peer certificate: UnknownIssuer` — the proxy re-signs all traffic —
  and the same call succeeds once the runtime's CA is added. The bootstrap
  therefore takes the CA the runtime itself declares, preferring
  `$SSL_CERT_FILE`, then `$NODE_EXTRA_CA_CERTS`, then `~/.ccr/ca-bundle.crt`,
  and only then the system bundle: some images install the proxy CA into
  `/etc/ssl/certs/ca-certificates.crt` and others do not, so the system bundle
  is a fallback rather than the answer. dira itself still never reads those
  ambient variables — the bootstrap (an operator-committed provisioning
  script, not `dira` itself) promotes one into the explicit opt-in, inside a
  detected cloud runtime, where a declared CA file is the intended signal; see
  DIRASH-0033's "Provisioning exemption" section.
- Reaching the Dira cloud additionally needs the runtime's **egress policy** to
  allow the host: a default Claude Code cloud environment answers `CONNECT`
  for `app.dirahq.sh` with `403`, independently of TLS trust. That is an
  environment setting (Custom network access), not something the repo can fix.
- Ephemerality: `sync_debounce_secs` / `sync_backstop_secs` are config knobs
  (defaults 3/90); the bootstrap exports `DIRA_SYNC_BACKSTOP_SECS=15` so an
  abruptly reclaimed VM loses at most ~15s of tail. Correctness under abrupt
  death was already the model: WAL store, idempotent batches, per-chunk
  cursors (D-0020).
- `dira doctor` gains `cloud.runtime` (detected runtime + provisioning env —
  `skip`, not `ok`, on a plain non-cloud machine, symmetric with
  `cloud.bootstrap`; its `DIRA_EXTRA_CA_CERTS`-readability arm runs *before*
  the runtime gate, so an unreadable bundle is reported on any machine, not
  only inside a detected runtime), `cloud.reachability` (one GET of
  `/api/v1/meta`, but **only when a cloud runtime is detected or the caller
  names `cloud.reachability` explicitly via `--check`** — never on a bare
  `dira doctor` on a plain machine, an explicit, recorded exemption from
  D-0006 scoped to this one check; `connect_timeout(1500ms)`, request
  `timeout(2s)`; a TLS failure names `DIRA_EXTRA_CA_CERTS`, other transport
  failures name the egress allowlist, and the verdict is `warn`, never `fail`,
  on any transport error — an unreachable cloud is not broken local capture),
  and `cloud.bootstrap` (artifact completeness + the version pin). Absent
  evidence skips (DIRASH-0022); a VM without a runner token warns. The
  `hooks.scope_overlap` check (not cloud-specific, but load-bearing for the
  yield story above) judges the same harness-wiring facts `hooks.config`/
  `hooks.exe_path` read — see the portable-hook-yield bullet above and
  `.zavet/specs/doctor.md`. No `cloud.*` or `hooks.scope_overlap` check ever
  returns `fail`.
- Verifying a cloud VM's capture with `dira status --json` (`cli/dira/src/
  render.rs`'s `print_json`) keeps its one-object-on-stdout contract on
  failure too: a `Response::Error` from the daemon still emits
  `{"status":"error","message":…}` on stdout with exit 1, never a bare stderr
  line. A daemon that cannot be reached at all is a separate, earlier
  failure — the socket connect itself fails before there is any `Response` to
  render — so that case goes to stderr with exit 1 and prints no JSON at all;
  `dira sessions --json` follows the same split.

## Invariants

- The wire stays content-free (D-0001): the runtime marker shipped in schema
  1.4 (`SessionRollup.runtime` + `runtimeSessionRef`) is a runtime *name* and
  a harness session id — metadata, never content. The daemon stamps both at
  flush time from `dira_core::runtime::detect` (they are a property of the
  running environment, not of stored events, and batch ids never derive from
  session rollups, so the post-assembly stamp is safe); outside a cloud
  runtime the keys stay off the wire, byte-identical to 1.3.
  **Caveat**: because the batch id derives only from events/artifacts/
  intervals, never from the session rollups the runtime stamp lives on, the
  stamp is invisible to the cloud's batch-id dedup. A window already flushed
  (and acked) before the runtime marker became detectable carries no
  `runtime`/`runtimeSessionRef` and stays that way permanently — a later
  `dira device resync` reforms the identical batch id for that same window
  and dedups as a no-op against the copy already stored, even though the
  re-sent payload would now carry the stamp. There is no re-flush path that
  retroactively corrects an already-acked window's runtime attribution.
- A committed hook config may never contain a machine-specific path, and never
  depends on an ambient variable being set: the repo-root anchor is spelled
  `${CLAUDE_PROJECT_DIR:-.}` so a harness build that does not export it falls
  back to the working directory instead of resolving to `/.dira/…`.
- Generated scripts are POSIX sh (no bashisms), and hook.sh's exit status is
  0 on every path.
- `DIRA_RUNNER_TOKEN`'s value never appears in diagnostics, logs, or facts, or
  on any subprocess's argv — only its presence.
- The portable hook yields, it never dedups: no cache, no time window,
  re-derived on every invocation from the two config files already on disk —
  the user-scope settings file and the project-scope one, both required to
  agree before a yield happens (DIRASH-0037). `dira doctor --probe`'s
  synthetic hook is structurally excluded from the yield branch (`!probe`),
  so the capture probe always drives the direct form and can never pass on
  the exact machine a real wiring bug is on — see DIRASH-0023 for the
  identical reasoning applied to a different code path. See DIRASH-0037.
- `cloud.reachability` never performs its GET outside a detected cloud runtime
  or an explicit `--check cloud.reachability`, and never returns `fail` — the
  D-0006 exemption is scoped to that one probe.

## Runner-token endpoint contract (for dirahq-cloud)

The CLI ships this claim variant dark until the cloud implements it:

- `POST /api/v1/devices/claim` with body
  `{ "runnerToken": string, "ed25519Pubkey": string, "label": string|null,
  "clientNonce": string }` (the `code` field is absent — its presence selects
  the interactive variant).
- Runner tokens are minted and revoked on the dashboard's Connections page,
  scoped to a workspace (recommended: per cloud environment). Anyone holding
  the token can mint devices into that workspace, so it is shown once (only a
  hash is stored) and revocable. Revocation is **fail-closed**: it stops new
  claims AND rejects further ingest/presence from every device the token
  minted — the kill switch for a leaked credential. Already-synced data is
  untouched. Claim errors are JSON: `404 {"error":"invalid_token"}` for an
  unknown token, `403 {"error":"revoked_token"}` for a revoked one; the CLI
  surfaces them verbatim.
- A successful claim returns `{ "deviceId": string }` for an
  **ephemeral device** bound to the token (`devices.kind = 'runner'`,
  `runner_token_id` set). Runner devices never appear individually in the
  dashboard's devices list — they are grouped read-time under their token's
  row (label, active/lifetime device counts) — so a fleet of dead VMs can't
  blow up Connections/settings. Device rows are kept forever regardless
  (proof signatures verify against the stored pubkey); hygiene is a display
  concern, not a deletion sweep.
- Idempotency: `(runnerToken, clientNonce)` collapses a retried claim onto
  the same device row, mirroring `(code, clientNonce)`.
- Unknown-field tolerance: an older cloud must reject an unrecognized claim
  body with a 4xx and a JSON `{"error": ...}`; the CLI surfaces it verbatim.

## Checks

- generated scripts substituted + POSIX + contracts ::
  cargo test -p dira --bin dira -- cloud_init::tests::generated_scripts_are_substituted_and_shell_sane
- portable commands read as wired by the shared matcher ::
  cargo test -p dira --bin dira -- init::reader_tests::portable_wrapper_commands_read_as_wired
- injection replaces absolute-path entries, converges ::
  cargo test -p dira --bin dira -- cloud_init::tests::nested_injection_replaces_absolute_path_entries_and_is_idempotent
- end-to-end artifacts + idempotency + dry run + git-toplevel refusal + malformed-config refusal + gitignore warning ::
  cargo test -p dira --test cloud_init_e2e
- digest pinning: no-pin skips the fetch, a good pin survives a failed re-fetch, a pin is never reused for a different version ::
  cargo test -p dira --bin dira -- cloud_init::tests::resolve_digests_no_pin_never_fetches_and_is_empty cloud_init::tests::resolve_digests_keeps_a_good_pin_on_a_failed_refetch cloud_init::tests::resolve_digests_does_not_reuse_a_pin_for_a_different_version cloud_init::tests::fetch_release_digests_embeds_both_targets_from_a_scripted_server
- `dira init` over a cloud-wired project adds nothing at project scope, `--global` still writes ::
  cargo test -p dira --test cloud_init_e2e -- dira_init_over_a_cloud_init_repo_adds_nothing_and_global_still_writes
- merge mode treats the portable wrapper as already wired, replace mode still matches exactly ::
  cargo test -p dira --bin dira -- init::reader_tests::merge_mode_treats_a_portable_wrapper_as_already_wired init::reader_tests::replace_mode_does_not_treat_an_arbitrary_wrapper_spelling_as_current
- the portable hook yields to a live, resolvable, catch-all user-scope entry; a tool-scoped matcher or another portable wrapper never counts; the marker alone never yields without a genuine project-scope portable wrapper too; a direct invocation still forwards ::
  cargo test -p dira --bin dira -- hook_yield::
- a real portable invocation yields, a direct one still forwards, a stray marker with no project-scope portable wiring still forwards, against real config files ::
  cargo test -p dira --test hook_yield_e2e
- runner claim: no TTY, token on the wire, server-assigned id, the runner token wins over a code when both are given, a retried claim reuses its nonce and clears it only on success ::
  cargo test -p dira --bin dira -- device::tests::runner_token_link_claims_without_a_tty device::tests::link_prefers_the_runner_token_over_a_code_when_both_are_given device::tests::a_retried_claim_reuses_the_same_client_nonce_and_clears_it_on_success
- runtime detection is conservative, an explicit override never carries a stray session ref, both fields clamp to 64 chars on a char boundary ::
  cargo test -p dira-core --lib -- runtime::
- extra CA roots never brick the client, a bundle with one corrupt block still loads the valid certs alongside it ::
  cargo test -p dira-core --lib -- httpclient::
- `DIRA_IDENTITY_EMAIL` needs an `@` and a plausible length, else falls through to git config ::
  cargo test -p dira-core --lib -- project::tests::env_identity_email
- sync cadence knobs clamp and default to the historical constants, `DIRA_SYNC_BACKSTOP_SECS` reaches `sync_backstop()` ::
  cargo test -p dira-core --lib -- config::tests::sync_cadence config::tests::env_sync_backstop_secs_reaches_sync_backstop
- cloud doctor judges ::
  cargo test -p dira --bin dira -- doctor::checks::tests::a_cloud
- a real agent session in a simulated cloud runtime is captured ::
  sh .github/scripts/cloud-capture-smoke.sh
- 1.4 runtime fields round-trip and stay off pre-1.4 wires ::
  cargo test -p dira-contract -- session_runtime_roundtrips
- the daemon stamps rollups only inside a detected cloud runtime ::
  cargo test -p dirad --lib -- rollups_carry_the_cloud_runtime
- bootstrap template is syntactically valid POSIX sh and shellcheck-clean ::
  sh -n cli/dira/templates/dira-bootstrap.sh && shellcheck -s sh cli/dira/templates/dira-bootstrap.sh
