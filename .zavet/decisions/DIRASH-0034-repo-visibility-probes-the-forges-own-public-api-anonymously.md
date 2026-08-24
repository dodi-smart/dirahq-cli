---
id: DIRASH-0034
title: Repo-visibility probing sends the plaintext ref only to the forge's own public API, anonymously
status: active
guards:
  - cli/dirad/src/repo_visibility.rs
checks:
  - visibility mapping, caching, and probe bounding hold :: cargo test -p dirad --lib repo_visibility
origin: recorded
verified: true
---

## Decision

WP3 resolves a GitHub/GitLab remote's Public/Private visibility with a cache-first,
anonymous `GET` to that forge's own public API (`api.github.com` /
`gitlab.com/api/v4`), carrying the plaintext `owner/repo` path. Bitbucket and
self-hosted remotes get `Unknown` with no request. This is the one place beyond
the local control socket (DIRASH-0033) the plaintext canonical ref travels to.

## Why

DIRASH-0033 guards "plaintext repo identity ... never sent to the network" against
Dira's own cloud ingest — that boundary is unchanged: `/api/v1/pulse` still only
ever receives `repo_hash`, `host_class`, and the resolved visibility string, never
the ref. The forge probe is a different network and a different question: the
forge already hosts the repo and already knows it exists, so asking it "is this
public?" discloses nothing to it that it doesn't already have. The request carries
no auth header, no cookie, and no install/device identifier — only a generic
`dirad/<version>` UA required by GitHub's API — so the forge cannot correlate the
request back to this install even if it wanted to. Visibility is a materially
useful segmentation signal for the pricing-strategy goal DIRASH-0033 already cites.

## Rejected

- Always reporting `Unknown` (never probing) — keeps the DIRASH-0033 boundary
  literally untouched, but throws away a real segmentation signal for an exposure
  that is, at most, "the forge learns someone anonymous asked about a repo it
  already hosts" — not a meaningful privacy cost.
- Routing the probe through Dira's cloud (cloud resolves visibility server-side) —
  rejected as WP3 scope creep; would need the cloud to hold forge credentials and
  widen the trusted-cloud surface for a lookup the daemon can do statelessly.

## Agent directives

- Never add an `Authorization`/cookie header, or any install/device identifier, to
  a request built in `repo_visibility.rs`.
- Never persist the plaintext canonical ref from this module — only cache by the
  salted `repo_hash` (`VisibilityCache`), matching DIRASH-0033's key discipline.
- Bitbucket/self-hosted must stay probe-free (`Unknown`, no request) unless a
  later decision adds a probe for them explicitly.

## Verification

`cargo test -p dirad --lib repo_visibility` covers the status→visibility mapping,
TTL choice (short for rate-limit/error, long for a confident or never-probed
answer), cache eviction/expiry, in-flight probe bounding, and the
`ingest`-integration "unknown first, real answer once warm" behavior. Whether the
request itself carries no auth/cookie headers is not separately asserted by a
test — a human reviewing `repo_visibility.rs`'s `request_visibility` is the check.
