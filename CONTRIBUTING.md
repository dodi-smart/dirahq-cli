# Contributing to Dira

Thank you for your interest in contributing. This repository is the **open-source** Dira
CLI (the `cli/` and `contract/` directories), licensed under Apache-2.0. The hosted Dira
cloud is proprietary and lives in a separate repository.

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
