# dirahq-cli — contributor & agent guide

This is the **open-source** Dira CLI: the Rust workspace producing the `dira` + `dirad`
binaries (`cli/core` + `cli/sources` libs) and the `contract` crate (the source-of-truth
wire schema). Licensed under **Apache-2.0** (see [`LICENSE`](LICENSE) / [`NOTICE`](NOTICE)).

The hosted **Dira cloud** is a separate, proprietary repository (`dodi-smart/dirahq-cloud`)
and is not part of this project.

## Conventional commits (required)

Every commit message MUST follow Conventional Commits and is enforced by the `commit-lint`
CI job (`commitlint.config.mjs`, `wagoid/commitlint-github-action`).

```
type(scope): subject
```

- **type** — one of: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`.
- **scope** — mandatory (see table). The scope decides whether a commit triggers a release.
- **subject** — imperative, not Capitalized/UPPERCASE. Keep every line ≤ 180 chars.
- Breaking changes: add `!` (`feat(cli)!: …`) or a `BREAKING CHANGE:` footer → major bump.

### Scope → release map

| Scope | Triggers a release? | Notes |
|---|---|---|
| `cli` | yes | Rust binaries/libs |
| `daemon` | yes | `dirad` changes |
| `contract` | yes | wire schema (also re-vendored by the cloud repo) |
| `repo`, `deps` | yes | repo-wide / dependency changes |
| `ci`, `release` | no | workflows, tooling, release commits |

Scope is mandatory because releases are driven by `semantic-release-scope-filter`: a config
only counts commits whose scope is in its allowed list, so a missing/unknown scope releases
nothing.

## The contract is the source of truth

The wire schema is authored once in Rust under `/contract`. Two artifacts are derived and
**drift-gated in CI** — never hand-edit them:

- `contract/attestation.schema.json` (`just contract-schema`)
- `contract/testdata/signing-vector.json` — the deterministic cross-language signing fixture (`just vector`)

Run both with `just contract`. The cloud repo vendors these. See the `contract-sync` skill.

## Releases (automated — never bump versions by hand)

Releases are run by **semantic-release** ([`.releaserc.js`](.releaserc.js)):

- Tags are `v${version}`; changelog is `CHANGELOG.md`.
- The version source of truth is `[workspace.package].version` in `Cargo.toml`; `cargo
  set-version` bumps it. **Do not** hand-edit it.
- CLI binaries (`dira`, `dirad`) are cross-compiled and attached to the GitHub release by
  `.github/workflows/build-release.yml`.

## Trunk-based development

- `develop` is the integration trunk and the **prerelease channel** → `…-develop.N` versions.
- `main` is **stable** → clean `x.y.z` versions; `main` is auto-merged back into `develop`.
- Work on short-lived `feat/…`, `fix/…` branches → PR into `develop` → promote `develop → main`.

## Local checks before pushing

- `just ci` (fmt + clippy + tests + contract schema + signing fixture).
- Verify the release config without publishing: `bunx semantic-release --dry-run --no-ci --extends ./.releaserc.js`.
- Contributions follow the DCO — sign off commits (`git commit -s`). See [`CONTRIBUTING.md`](CONTRIBUTING.md).
