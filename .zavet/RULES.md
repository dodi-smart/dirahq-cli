# Standing rules

Curated directives distilled from decisions. Every line here is injected into
agent context at session start — keep it short and non-negotiable.

- Never hand-edit generated contract artifacts (`contract/*.schema.json`,
  `contract/testdata/signing-vector.json`) — run `just contract`.
- Never bump `[workspace.package].version` by hand — releases are
  semantic-release's job.
- Every commit uses a conventional `type(scope): subject` with a mandatory
  scope, and is DCO signed off (`-s`).
- Nothing content-bearing (prompts, diffs, bodies, messages) may cross the
  attestation wire — metadata only. See D-0001.
