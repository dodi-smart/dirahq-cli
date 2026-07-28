---
id: D-0007
title: The landing site vendors the installers byte-for-byte; it does not proxy them
status: active
guards:
  - install.sh
  - install.ps1
  - .github/workflows/sync-install-script.yml
origin: recorded
verified: true
---

## Decision

`install.sh` and `install.ps1` at this repo's root are the source of truth.
The landing site carries byte-identical copies at `scripts/install.sh` and
`scripts/install.ps1` and serves them from `/install` (+ `/install.sh`) and
`/install.ps1`. A workflow here opens a PR against the landing repo whenever
either script changes, and a scheduled job diffs the served URLs against
these copies.

History note: the sync workflow originally prepended a two-line provenance
header to the vendored copy, contradicting this record and the landing
repo's own README (both said byte-identical, and the actual landing bytes
had no header). The workflow was the bug; it now does a plain `cp`. Do not
reintroduce a header.

## Why

A Vercel rewrite to `raw.githubusercontent.com` was the obvious design and
does not work. `@astrojs/vercel` writes `.vercel/output/config.json` itself
via the Build Output API, so a root `vercel.json` is read only to warn about
a `trailingSlash` conflict — its routing and headers are silently ignored.
The rewrite would appear configured and do nothing.

Even if it worked it would be wrong here: it fails entirely while this repo
is private, which is the whole pre-launch testing window; it puts
githubusercontent's availability and rate limits in the install path; and it
serves whatever is on the branch right now, so a bad commit is live with no
deploy gate and no rollback.

The copy is byte-identical — the "vendored, do not edit" notice lives in a
sibling README, not a header comment — specifically so the drift check is a
plain `diff` with nothing to normalize. A header would mean defining "drift
modulo N lines", which is a rule that eventually gets it wrong.

## Rejected

- **Vercel rewrite or proxy to raw.githubusercontent.com** — silently
  ignored by the adapter, and broken while the repo is private.
- **Astro fetching the script at build time** — same private-repo problem,
  and it makes the landing build depend on a GitHub fetch.
- **A vendored copy with an added provenance header** — breaks byte equality
  and complicates the drift check for a comment.

## Agent directives

- Edit `install.sh` here only. Never edit the landing repo's copy directly.
- Keep the two files byte-identical; provenance notes go in
  `scripts/README.md` on the landing side.
- Do not add routing or headers to a `vercel.json` in the landing repo and
  expect them to apply — the adapter owns the output config.
- The release also attaches `install.sh` as an asset, so
  `releases/latest/download/install.sh` stays a permanent fallback URL.
