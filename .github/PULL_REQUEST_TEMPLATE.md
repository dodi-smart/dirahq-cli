## Summary

<!-- What does this change, and why? -->

## Test plan

- [ ] `just ci` passes (fmt + clippy + tests + contract schema + signing fixture)
- [ ] If this changed anything under `/contract`: `just contract` was run and the
      regenerated artifacts are committed in the same commit (never hand-edited)
- [ ] If this changed the signed byte stream: the cross-language vector still verifies
      against the cloud's TS verifier
- [ ] If this changed `install.sh` / `install.ps1`: linted clean
      (`shellcheck -s sh install.sh`, `shfmt -d -s -ln posix -i 2 install.sh`,
      `Invoke-ScriptAnalyzer -Path install.ps1 -Severity Warning`)

## Checklist

- [ ] Commits are signed off (`git commit -s`) per the
      [DCO](../CONTRIBUTING.md)
- [ ] Commit messages follow Conventional Commits with a **mandatory scope** from
      `cli, daemon, contract, ci, release, repo, deps` — the scope decides whether a
      release is cut (see [CLAUDE.md](../CLAUDE.md))
- [ ] This PR targets `develop` (the integration trunk), not `main`
- [ ] Capture stays metadata-only — no prompt text, file contents, or diffs added to the
      store, an event, or an attestation
