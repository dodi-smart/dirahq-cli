---
id: DIRASH-0033
title: Telemetry is opt-out, anonymous by construction, and rides its own unsigned channel
status: active
guards:
  - cli/core/src/telemetry/
  - cli/dira/src/telemetry.rs
  - cli/dirad/src/telemetry_sync.rs
checks:
  - every wire variant carries exactly its declared keys :: cargo test -p dira-core --lib telemetry::event
  - the disclosure names what ships :: cargo test -p dira --bin dira the_telemetry
origin: session
verified: false
---

## Decision

Product analytics is **on by default** (opt-out), disclosed by its own prompt
in onboarding and by a one-time first-run notice, and disabled by any of:
`telemetry.enabled = false`, `DIRA_TELEMETRY_ENABLED=0`, `DO_NOT_TRACK=1`,
`CI`, or a dev build. What ships is the closed `TelemetryEvent` enum and
nothing else: command name (top-level only), duration, success plus a closed
error-kind taxonomy, host class, visibility, and a salted repo hash. Never
argv, paths, repo names, git identity, or error text.

Identity is a **random install ULID plus a random 32-byte salt**, minted
lazily in `meta`, never derived from the Ed25519 device key. The repo hash is
HMAC-SHA256 keyed by that per-install salt, computed **daemon-side**: the
canonical `host/owner/repo` ref crosses only the local control socket, and
plaintext repo identity is never persisted in the queue nor sent to the
network. Batches are **unsigned** and flush to the cloud's `/api/v1/pulse`
gated on `cloud_url` + consent — explicitly never on device linkage.

## Why

**Opt-out with honest disclosure** is the only consent model that yields data
representative enough to steer a pricing strategy, and it stays honest the
same way DIRASH-0030 does: a named disclosure constant shown on every path,
pinned by a wording test to the fields that actually ship, so the promise and
the payload cannot drift apart silently.

**The identity split is the load-bearing part.** Reusing the device key (or
anything derived from it) as an analytics id would let the analytics store
correlate back to the signing identity, and would break the moment a key
rotates. A per-install salt keyed into the repo hash means the same repo
hashes differently on every install: we can count distinct repos per install
and split public from private, but no cross-install correlation of repos is
possible even with our own database in hand — which is what lets the word
"anonymous" in the disclosure be true rather than aspirational.

**Unsigned, and consent-gated rather than link-gated**, because the entire
point is hearing from installs that never linked. Requiring the envelope
would silence exactly the population whose conversion we want to understand,
and telemetry is not trust-critical: the cloud treats it as untrusted input
behind a server-side allowlist regardless of what we sign.

**One final `consent_recorded(enabled=false)`** is allowed through when the
knob is turned off — the opt-out rate is itself the signal that keeps this
feature honest — but every other kill switch (env, DO_NOT_TRACK, CI, dev
build) suppresses even that.

## Rejected

- Authoring the wire types in `/contract` — telemetry is best-effort and
  versioned independently (`v: 1`); riding the drift-gated contract would
  couple every taxonomy tweak to a contract release and the cloud vendoring
  dance.
- A global (unsalted) repo hash — would let public repos be dictionary-
  reversed and private repos be correlated across installs; "pseudonymous"
  is not what the disclosure says.
- Emitting from the CLI process directly — D-0006's rule generalizes: no
  network on the foreground path. The CLI's only telemetry I/O is a
  150ms-budgeted local-socket fire-and-forget.
