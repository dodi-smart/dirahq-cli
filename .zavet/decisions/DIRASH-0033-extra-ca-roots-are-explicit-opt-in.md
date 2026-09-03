---
id: DIRASH-0033
title: Extra CA roots are an explicit, additive opt-in — the bundled store is never swapped
status: active
guards:
  - cli/core/src/httpclient.rs
  - cli/dirad/src/lib.rs
  - cli/dira/src/device.rs
  - cli/dira/src/update/mod.rs
  - cli/dira/src/doctor/mod.rs
  - cli/dira/templates/dira-bootstrap.sh
checks:
  - a missing or malformed bundle never bricks the client :: cargo test -p dira-core --lib httpclient::tests::unreadable_or_garbage_bundle_never_bricks_the_client
  - unset/blank env yields the default (bundled-roots-only) client :: cargo test -p dira-core --lib httpclient::tests::unset_or_blank_env_yields_default_builder
  - a bundle with one corrupt block still loads the valid certs alongside it :: cargo test -p dira-core --lib httpclient::tests::partial_bundle_loads_the_valid_cert_and_skips_the_corrupt_one
origin: session
verified: true
---

## Decision

Every device→cloud `reqwest::Client` is constructed through
`dira_core::httpclient::builder()`. When `DIRA_EXTRA_CA_CERTS` names a readable
PEM bundle, each certificate in it is **added** to the binary's bundled Mozilla
root set; the bundled store itself is never replaced, and no other source of
extra trust is consulted — deliberately not `SSL_CERT_FILE` or the OS store.
An unset/blank variable, an unreadable file, or an unparsable bundle all yield
the default client with a warning, never an error.

This amends D-0011 (TLS roots ship in the binary); it does not supersede it.
Trust remains a property of the artifact — an operator may *append* anchors
for one process, never swap the store out from under it.

## Why

Cloud agent runtimes (Claude Code on the web, Cursor cloud agents) route all
egress through a TLS-intercepting security proxy whose CA exists only inside
the ephemeral VM. Under D-0011's bundled-roots-only posture, every HTTPS call
to the Dira cloud (and to GitHub, for `dira update`) fails verification there.
The narrowest fix is a per-process, explicit, additive anchor: visible in the
environment, auditable, and inert everywhere it isn't set. Honoring ambient
variables like `SSL_CERT_FILE` would silently widen trust whenever an
unrelated toolchain exported them; requiring an explicit `DIRA_`-namespaced
opt-in keeps the decision with the operator.

The never-brick posture mirrors `identity::env_key`: a typo in an env var must
degrade to the default behavior, not take sync offline harder than the proxy
already does.

## Provisioning exemption: the bootstrap may promote a runtime-declared CA

`dira` itself — the compiled binary, at any call site — still never reads an ambient variable
to decide what to trust; that boundary is absolute. What is new is *who* is allowed to set
`DIRA_EXTRA_CA_CERTS` in the first place. `cli/dira/templates/dira-bootstrap.sh`, the
repo-committed SessionStart script `dira cloud init` generates, is not `dira`: it is the
operator's own provisioning step, running before `dira`/`dirad` exist as processes at all, and
it is reviewed and committed by whoever ran `dira cloud init`. Inside a runtime this bootstrap
has already confirmed is a detected cloud runtime, it may read the CA file the runtime itself
declares (preferring `$SSL_CERT_FILE`, then `$NODE_EXTRA_CA_CERTS`, then
`~/.ccr/ca-bundle.crt`, then the system bundle as a last resort) and *promote* it into the
explicit, `DIRA_`-namespaced opt-in before starting the daemon. That is not `dira` widening its
own trust — it is the operator's committed shell script making the same choice a human would
have made by hand, once, at the one boundary (cloud-VM provisioning) where nobody is present to
make it interactively. `dira doctor`'s `cloud.runtime` check (`cli/dira/src/doctor/mod.rs`)
reads the resulting `DIRA_EXTRA_CA_CERTS` the same way every other check does — as ambient
environment, already set by something else — and reports on its readability; it does not itself
promote anything.
