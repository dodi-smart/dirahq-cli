---
id: D-0007
title: The landing site vendors install.sh byte-for-byte; it does not proxy it
status: active
guards:
  - install.sh
  - .github/workflows/sync-install-script.yml
origin: recorded
verified: true
---

## Decision

`install.sh` at this repo's root is the source of truth. The landing site
carries a byte-identical copy at `scripts/install.sh` and serves it from
`/install`. A workflow here opens a PR against the landing repo whenever the
script changes, and a scheduled job diffs the served URL against this copy.

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
