---
name: contract-sync
description: >-
  Regenerate and verify Dira's contract artifacts after changing the wire schema. Use
  whenever a Rust type under /contract (the serde + schemars source of truth) changes, or
  when CI reports schema/fixture drift. Emits the JSON Schema and the cross-language signing
  fixture, then checks for drift the way CI does. This repo is the PRODUCER; the cloud repo
  vendors these artifacts.
---

# contract-sync

The wire schema that crosses the daemon↔cloud boundary is authored **once** in Rust under
`/contract` (because the daemon is the producer). This repo owns two derived artifacts, and
CI fails if either is stale:

1. `contract/attestation.schema.json` — emitted from the Rust types.
2. `contract/testdata/signing-vector.json` — a deterministic Ed25519 signing fixture the
   cloud's TS verifier must accept (byte-for-byte JCS parity).

The cloud lives in a **separate repo** (`dodi-smart/dirahq-cloud`) and vendors both files,
then generates its own TS types + Zod from the schema. Never hand-edit either artifact.

## Regenerate

From the repo root:

```sh
just contract            # = contract-schema + vector
```

Which runs:

- `cargo run -q -p dira-contract --bin emit-schema` → rewrites `attestation.schema.json`
- `cargo run -q -p dira-core --bin sign_vector > contract/testdata/signing-vector.json`

Run pieces individually with `just contract-schema` / `just vector`.

## Verify (mirror the CI drift gates)

```sh
git diff --stat contract/attestation.schema.json contract/testdata/signing-vector.json
```

- **Clean diff after regenerating** = in sync. Commit the regenerated artifacts alongside
  the Rust change in the same commit, scoped `contract`.
- **Non-empty and unexpected** = your Rust change altered the wire shape (or the signed byte
  stream). That's fine — just commit the regenerated artifacts, or CI's two drift gates
  (`contract schema is up to date`, `signing vector fixture is up to date`) will fail.

## Propagating to the cloud

After a `contract`-scoped change lands here, the cloud repo must re-vendor. With both repos
checked out as siblings, run `just contract-pull` in `dirahq-cloud` (it copies these two
files from `../dirahq-cli` and regenerates the cloud TS/Zod). See the cloud repo's
`contract-sync` skill. If the cross-language signing changed, also consult the
`signing-reviewer` subagent.
