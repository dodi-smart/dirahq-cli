---
id: D-0011
title: TLS trust anchors ship inside the binary, never the host trust store
status: active
guards:
  - Cargo.toml
  - .github/renovate.json5
origin: recorded
verified: true
---

## Decision

The HTTP client is `reqwest` with `default-features = false` and `rustls-tls`:
a `ring`-backed rustls validating against the Mozilla roots compiled into the
binary. `reqwest` is held below 0.13 in `Cargo.toml` and in Renovate's
`allowedVersions`, because 0.13's `rustls` feature changes both halves of that.

## Why

Linux ships as one static musl binary (D-0002) that has to work on a host we
know nothing about. Bundled roots make TLS a property of the artifact; the host
trust store makes it a property of the box. A minimal container or a distro
without `ca-certificates` turns a working release into a runtime failure on the
daemon→cloud path, far from anything CI or the installer can see.

reqwest 0.13's `rustls` feature swaps the crypto provider to `aws-lc-rs` (a C/asm
build that lands in the musl cross-compile) and routes validation through
`rustls-platform-verifier`, the host store. 0.13.4 exposes no feature that
restores bundled roots. Its whole TLS feature set is `rustls`,
`rustls-no-provider`, and the native-tls family. 0.12's `webpki-roots` is gone.
So the migration means a direct `webpki-root-certs` dependency fed through
`tls_certs_only`, plus an explicit `ring` provider install, plus a release-matrix
build to prove the musl legs still link. Its own change, not a line in a batch.

## Rejected

- **Take 0.13 with `rustls` (the default)**. Silently trades bundled roots for
  the host store and adds a C toolchain to the static musl legs; both failures
  surface in the field, not in CI.
- **Take 0.13 with `rustls-no-provider`**. Correct shape, but incomplete on its
  own: it panics at client construction until a provider is installed, and still
  needs the root store wired up by hand. Do it deliberately or not at all.
- **`native-tls`**. A system OpenSSL dependency is the opposite of a static
  binary that runs anywhere.

## Agent directives

- Do not bump `reqwest` past 0.12 as part of a dependency batch. It needs its own
  PR that also lands the root store, the provider install, and a green release
  build for both musl targets.
- If that migration happens, supersede this record. Do not edit it.
- Keep `default-features = false` on `reqwest`: the default feature set pulls
  `default-tls`, which is how an OpenSSL dependency sneaks back in.
