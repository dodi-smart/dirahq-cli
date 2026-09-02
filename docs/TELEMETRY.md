# Telemetry

Dira collects anonymous product-usage analytics, on by default. This document says
exactly what that means: what is collected, what never is, where it goes, and every way
to turn it off.

This is a separate channel from **knowledge sync** (decision and spec content — see
[zavet.md](zavet.md)) and from **cloud sync** (attestation batches for billing). Turning
telemetry off touches none of those, and turning them off does not touch telemetry.

## Why it exists

Dira is a small team building a CLI that runs unattended on other people's machines.
Telemetry answers two questions we would otherwise have no way to answer: which commands
and harnesses are actually used, and where the CLI fails in the wild. It exists for
product sustainability and prioritization — deciding what to fix and what to build next —
not for surveillance of any individual's work. It carries no marketing or resale purpose.

## What is sent

Every event carries a fixed, closed set of fields — never a prompt, a free-text value, or
anything not listed below.

| Event | Sent when | Properties |
|---|---|---|
| `cli_command_executed` | a CLI or daemon-served command finishes | `command` (e.g. `"status"`, `"config"` — the top-level name only, never a sub-action and never raw argv), `duration_ms`, `success`, `error_kind` (one of a fixed set: `daemon_unreachable`, `daemon_error`, `invalid_input`, `io_error`, `timeout`, `internal`), and — only when cwd is inside a git repository — `repo_host_class` (`github`/`gitlab`/`bitbucket`/`self_hosted`), `repo_visibility` (`public`/`private`/`unknown`), `repo_hash` |
| `cli_daemon_started` | `dirad` finishes starting up | none beyond the base fields below |
| `cli_daemon_stopped` | `dirad` shuts down | `duration_ms` (uptime in ms) |
| `cli_consent_recorded` | the telemetry toggle changes | `telemetry_enabled`, `consent_source` (`prompt`/`yes_flag`/`config_set`) |

Every event also carries a timestamp, the running `dira`/`dirad` version, OS, and CPU
architecture.

## What is never sent

- Repo names, owners, or URLs — never to Dira. (Working out `repo_visibility` sends
  the plaintext `owner/repo` to the repo's own host, not to Dira — see the next
  section.)
- Git identity — author name or email
- File paths, file contents, or diffs
- Command arguments or flag values
- Error messages or any other free text
- Anything that identifies you as a person, as opposed to an anonymous install

## How public/private is determined

To resolve `repo_visibility`, the daemon asks the forge that already hosts the
repo — an unauthenticated GitHub or GitLab API lookup of `owner/repo` — because
that forge already knows the repo exists; the lookup discloses nothing to it
that it doesn't already have. The plaintext name goes only to that provider,
never to Dira's servers, and the request carries no auth token, cookie, or
install/device identifier — a generic `dirad/<version>` user agent is the only
thing beyond the bare lookup. Any other host (Bitbucket, self-hosted) is
reported as `unknown`, with no request made at all. See DIRASH-0034 for the
full rationale.

## The repo hash

`repo_hash` lets the same repository be recognized across events from **one install**
without ever naming it: it is `HMAC-SHA256(per-install salt, canonical remote)`, hex
encoded. The salt is generated once per install, stored locally, and never transmitted.
Two installs hashing the same repository produce unrelated hashes, and the hash cannot be
reversed back to the repo without the salt — so repos can't be correlated across
different people's machines, and Dira cannot learn which repo it is from the hash alone.

`repo_visibility` is only ever `public` or `private` when something in the pipeline has
actually determined it; otherwise it is `unknown` rather than a guessed default.

## The install id

Events are tagged with a random `install_id`, generated once per machine and stored
locally — not derived from and not linked to your device's signing key or attestation
identity. It exists only to let Dira de-duplicate and count installs, not to identify a
device across the two systems.

**Linking a device is different.** Once you link this device to a cloud workspace
(`dira device link`), subsequent telemetry from that install may be associated with your
workspace account, the same way a signed-in product associates analytics with the account
once you sign in. This is the one place telemetry crosses from purely anonymous into
workspace-attributable, and it only happens after you take the linking action yourself.

## Where it goes

Telemetry is queued locally and flushed by `dirad` to Dira's cloud ingest endpoint, which
forwards it to PostHog's **EU** region. It never goes anywhere else, and it is not sold or
shared with third parties.

## Turning it off

Any of the following disables collection and sync entirely — no partial mode, no events
queued while off:

```sh
dira config set telemetry.enabled false   # persistent, in config.toml
DIRA_TELEMETRY_ENABLED=0                  # per-invocation or exported
DO_NOT_TRACK=1                            # the cross-tool convention
```

`dira onboard` also asks, in its own step, before any of the above — see below.

Dev builds (`cargo build` without a release profile) and CI runs never send telemetry,
regardless of the knob's value.

## The onboarding step

`dira onboard` shows this disclosure — the same content as this document, in short form —
before asking, on every path: the interactive prompt, `--yes`, and an explicit
`--telemetry <on|off>` flag all see it first. The default answer is on, matching
`TelemetryKnobs::default()`.

Accepting the default writes nothing to `config.toml` — an absent `[telemetry]` table
already means "on" for every pre-existing install, so recording the default would be
noise. Declining writes `telemetry.enabled = false` and confirms that nothing further will
be sent.

## Checking or changing it later

```sh
dira config get telemetry.enabled
dira config set telemetry.enabled off
```

Daemon-side changes take effect after a restart: `dira daemon stop` then `dira daemon
start` (or `dira daemon restart`).
