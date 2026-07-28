# Contributing to Dira

Thank you for your interest in contributing. This repository is the **open-source** Dira
CLI (the `cli/` and `contract/` directories), licensed under Apache-2.0. The hosted Dira
cloud is proprietary and lives in a separate repository.

By participating, you agree to abide by our [Code of Conduct](CODE_OF_CONDUCT.md).

**Found a security issue?** Don't open a public issue — see [SECURITY.md](SECURITY.md) for
private reporting. That includes anything that would put prompt text, file contents, or
diffs into the store or onto the wire: capture is metadata-only by design, and a breach of
that is a vulnerability, not a bug.

## Developer Certificate of Origin (DCO)

We use the [Developer Certificate of Origin](https://developercertificate.org/). By signing
off on your commits, you certify that you wrote the patch or have the right to submit it
under the project's Apache-2.0 license.

Add a sign-off line to every commit:

    git commit -s -m "feat(cli): add ..."

which appends:

    Signed-off-by: Your Name <your.email@example.com>

Pull requests whose commits are not signed off will be asked to amend.

## Ground rules

- Conventional commit messages with a mandatory scope; keep each line under 180 characters.
  See [CLAUDE.md](CLAUDE.md) for the scope → release map.
- Run `just ci` (fmt + clippy + tests + contract schema + signing fixture) before opening a PR.
- Never hand-edit the generated contract artifacts (`contract/attestation.schema.json`,
  `contract/testdata/signing-vector.json`) — regenerate with `just contract`.
- Contributions to these components are licensed under Apache-2.0.
- "Dira" and the Dira logo are trademarks of Dodi Smart OOD and are not licensed for use
  in derivative or competing products.

## Trunk-based development

Open PRs against `develop`. `main` is the stable release channel; `develop` is the
prerelease channel. Short-lived `feat/…`, `fix/…` branches → PR into `develop`.

## Releasing

Nobody bumps `Cargo.toml` by hand. `semantic-release` (`.releaserc.js`) inspects merged
commits' scopes (see the table in [CLAUDE.md](CLAUDE.md)) and, when one of them is
release-triggering, cuts a `v<version>` tag, a GitHub Release, and a `CHANGELOG.md` entry —
a prerelease (`…-develop.N`) on `develop`, a clean `x.y.z` on `main`.

`.github/workflows/build-release.yml` reacts once that Release workflow finishes and builds
the actual binaries across five matrix legs: `x86_64-unknown-linux-musl`,
`aarch64-unknown-linux-musl`, `universal-apple-darwin` (a `lipo` fat binary covering both
Apple Silicon and Intel), `x86_64-pc-windows-msvc`, and `aarch64-pc-windows-msvc` (native
on `windows-11-arm`, D-0014). Each produces `dira-<version>-<target>` plus a matching
`.sha256` — `.tar.gz` on unix, **`.zip` on Windows** — with an aggregate `checksums.txt`,
both installers, and the contract artifacts attached to the release. Seven smoke legs then
install each artifact for real and exercise the daemon lifecycle.

**Prereleases build automatically** (D-0013). A `develop` merge tags a prerelease and the
build workflow packages it exactly like a stable tag, so `--channel prerelease` is a real
dogfooding path. This used to be dispatch-only to avoid billed runner minutes on a private
repo; the repo is public, GitHub-hosted minutes are free, and only the
`x86_64-unknown-linux-musl` leg can land on the self-hosted pool at all.

### Dry-running a build

Exercise the whole pipeline (build + package, no uploads — no release assets, no
`checksums.txt`, no smoke job) against any existing tag:

```sh
gh workflow run build-release.yml -f tag=v0.1.0-develop.10 -f dry_run=true
```

Drop `dry_run` to actually publish artifacts for that tag. Prereleases now build on their
own, so this is a backfill for a tag that predates that change or whose build failed:

```sh
gh workflow run build-release.yml -f tag=v0.1.0-develop.10
```

### Smoke-testing a release by hand

The workflow's own `smoke` job runs `install.sh` for real against the tag it just built, on
five legs (linux-x64, linux-arm64, alpine, macos-arm64, macos-intel), and asserts version,
`dira daemon start`/`status`, `dira update --check`, `dira daemon restart`/`stop`, an
idempotent re-install, and `--uninstall`. To reproduce the same flow on a laptop with no
release cut at all:

```sh
just package       # build release binaries, tar + checksum them into dist/
just install-local  # run install.sh against dist/ via a file:// URL, into a scratch bin dir
```

See [docs/install.md](docs/install.md) for the full installer reference (every flag, every
`DIRA_*` env var, and `dira update`'s semantics).
