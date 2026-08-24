---
title: Telemetry — anonymous product analytics
version: 1
origin: session
verified: false
confidence: high
date: 2026-08-25
paths:
  - cli/core/src/telemetry/
  - cli/dira/src/telemetry.rs
  - cli/dirad/src/telemetry_sync.rs
  - cli/dirad/src/repo_visibility.rs
decisions: [DIRASH-0033, DIRASH-0030, DIRASH-0031, D-0006, D-0011, D-0020]
---

## Overview

Anonymous, opt-out product analytics: which commands run, how long they take,
and coarse facts about the repos they run in, flushed through the daemon to
the Dira cloud's `/api/v1/pulse` proxy and forwarded server-side to PostHog
Cloud EU. The user-facing disclosure is `docs/TELEMETRY.md`; the structural
rules are DIRASH-0033.

## Pipeline

1. **Emit (CLI, `cli/dira/src/telemetry.rs`).** The thin `main()` times the
   dispatched command and calls `record_command`, which passes
   `TelemetryGate` (knob, `DIRA_TELEMETRY_ENABLED`, `DO_NOT_TRACK`, `CI`,
   dev build — any one suppresses), classifies failure into the closed
   `ErrorKind`, resolves the cwd's canonical repo ref via `explain_project`,
   and fire-and-forgets `Request::IngestTelemetry` over the control socket
   under a 150ms total budget. The CLI process never does network I/O for
   telemetry (D-0006 generalized). Some paths exit the process directly and
   record nothing — listed on `run()`'s doc comment.
2. **Ingest (daemon, `telemetry_sync::ingest`).** Re-checks consent, mints or
   loads the install id + salt (`meta`), hashes the canonical ref
   (HMAC-SHA256, per-install salt) with visibility from the probe cache
   (`Unknown` on a cold cache; the probe fills it for later events), and
   appends the finished wire JSON to the `telemetry_events` queue. Plaintext
   repo identity is never stored.
3. **Flush (daemon, `telemetry_sync::run`).** knowledge_sync-shaped loop:
   5s debounce, jittered 300s backstop, chunks of 200 over `(cursor, until]`,
   POST `{cloud_url}/api/v1/pulse` on the shared TLS-pinned client (D-0011),
   cursor advances per accepted chunk on its own 2xx (D-0020), backoff via
   the shared ladder (DIRASH-0031). Gate is `cloud_url` + consent — never
   device linkage. 400 advances past the poison chunk loudly (rows kept);
   404 is a quiet endpoint-missing skip; 413/429/5xx/network are transient.
   Health lands in `META_TELEMETRY_HEALTH`.
4. **Visibility probe (`repo_visibility.rs`).** Unauthenticated GET to the
   provider that already hosts the repo (github.com / gitlab.com only),
   200→public, 404→private, else unknown; cached 24h keyed by the salted
   hash, short-TTL on rate-limit/error; never blocks ingestion; no tokens.

## Consent surfaces

- Onboarding step (DIRASH-0030 shape): `TELEMETRY_DISCLOSURE` shown on every
  path, confirm defaults to on, decline writes `telemetry.enabled = false`,
  `--telemetry <on|off>` skips the prompt. A wording test pins the
  disclosure to the shipped fields.
- First-run notice: once, stderr, tty-only, marker file in the config dir.
- `dira config set telemetry.enabled on|off`, `DIRA_TELEMETRY_ENABLED`,
  `DO_NOT_TRACK`. Consent transitions emit `cli_consent_recorded`; turning
  the knob off is the one event allowed through on the disable transition.

## Identity

`telemetry_install_id` (ULID) + `telemetry_salt` (32 random bytes) in `meta`,
independent of the device key. `dira device link` sends the install id in the
claim body (fetched from the daemon, gate-checked, never blocking the link);
the cloud performs the PostHog alias at claim time. `Store::nuke` clears the
queue, cursor, and health keys.

## Invariants worth re-checking after changes

- Changing what ships requires updating `TELEMETRY_DISCLOSURE`,
  `docs/TELEMETRY.md`, and the cloud allowlist in the same change set.
- The wire enum's per-variant no-stray-field tests are the drift guard; a new
  wire field without a taxonomy decision should fail review.
- The batch is versioned `v: 1`; the cloud 400s unknown majors and the daemon
  skips past such batches — bump deliberately.
