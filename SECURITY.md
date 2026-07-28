# Security Policy

## Reporting a vulnerability

**Please report privately, not via a public issue.** Use GitHub's
[private vulnerability reporting](https://github.com/dodi-smart/dirahq-cli/security/advisories/new)
(Security tab → "Report a vulnerability") on this repository. That opens a private advisory
only maintainers can see, which we can turn into a coordinated fix and public disclosure
once one exists.

We'll acknowledge new reports within a few business days. There's no bug bounty; there is a
credit in the advisory and the changelog if you want one.

## Why this surface deserves care

`dirad` is a **resident daemon**. Once `dira daemon install` has run, it starts at login
(launchd / systemd-user) and stays up, listening on a control socket and a loopback HTTP
port, ingesting hook payloads from every agent session on the machine. `dira hook <harness>`
runs on **every tool call** in a wired harness. Anything that turns that into more than
"append metadata to a local SQLite store" is worth reporting.

Concretely, the areas where a bug matters most:

### Capture must stay metadata-only

Dira's core claim is that captured data is **policy-free and metadata-only** — durations,
counts, SHAs, session ids, tool names — and never prompt text, file contents, or diffs. The
repo is open specifically so you can audit that claim.

**A path that puts content-bearing data into the store, an event, or an attestation is a
security bug, not a feature request**, even if nothing is currently syncing it anywhere.
This is the single most valuable thing to look for here.

### The device key

Each device holds an Ed25519 keypair; the secret lives in the OS keychain (`keyring`) and
signs attestations. Only the public key and a server-issued `device_id` are ever meant to
leave the machine. Report any path that logs, prints, serializes into an event, writes to
disk, or syncs the secret.

### Ingress

`dirad` accepts hook payloads over a Unix domain socket (a fixed per-user path) and a
loopback HTTP route. Report anything that lets a non-loopback origin reach the HTTP ingress,
lets another local user reach the socket, or turns a malformed payload into something other
than a dropped event — in particular anything that reaches command execution, path traversal
outside the store, or a crash of the resident daemon.

### Install and self-update

`install.sh` / `install.ps1` are `curl | sh`-class installers, and `dira update` replaces
binaries in place. Both verify sha256 and replace by atomic rename. Report signature/digest
checks that can be skipped or downgraded, redirects to a non-release host, TOCTOU windows
around the rename, anything that writes outside the install prefix, or any path that lets an
update overwrite a development install (it is meant to refuse).

### Wire and verification

The attestation format is authored in `/contract` and signed over RFC 8785 (JCS) canonical
JSON. Report any way to make the Rust producer and a conforming verifier disagree on the
byte stream, or to get a payload accepted under a key that didn't sign it.

## Scope

This policy covers what's in this repository: the `dira` and `dirad` binaries, `cli/core`,
`cli/sources`, `cli/ipc`, the `contract` crate, and the installers.

Out of scope here:

- **The Dira cloud** (the hosted verify/billing/policy/dashboard service) is a separate,
  proprietary service. If you believe you've found something server-side, still report it
  through the link above — we'll route it — but please don't test against production
  beyond what an ordinary account does.
- **[zavet](https://github.com/dodi-smart/dirahq-zavet)**, the knowledge-layer plugin, has
  [its own security policy](https://github.com/dodi-smart/dirahq-zavet/security).
- **Claude Code** and other harnesses belong to their vendors — for Claude Code see
  [Anthropic's responsible disclosure policy](https://www.anthropic.com/responsible-disclosure-policy).

## What isn't a vulnerability

- The daemon binds a loopback HTTP port by design; that it's reachable from other processes
  belonging to the *same user* is the intended trust boundary, not a bug.
- `curl | sh` being `curl | sh`. If you'd rather not, `docs/install.md` documents downloading
  and verifying the release archive yourself.
- Rate limits, quotas, or abuse controls being enforced server-side rather than in the
  client. The client is untrusted by design.
