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
- Never open an installed binary for writing — stage beside it and `rename`
  onto it (on Windows: rename the running exe aside first), or you get
  `ETXTBSY` in production and green tests. See D-0003.
- Linux artifacts are static musl; macOS is one universal binary; Windows is
  two MSVC zip legs. Never add an arch branch on Darwin, a libc probe, or a
  Windows case to install.sh — Windows installs go through install.ps1.
  See D-0002/D-0010.
- Never create the Windows control pipe without its explicit security descriptor
  (user-only DACL + medium integrity label) — in `bind` **and** in the accept
  loop, or connection #2 silently loses it. Never report `ERROR_ACCESS_DENIED`
  as "daemon not running". See D-0016.
- `dira doctor` diagnoses and never acts: no `--fix`, and a check whose inputs
  are missing reports `skip`, never `fail`. Exit codes 0/1/2 are a contract.
  See DIRASH-0022.
- The capture probe's session id is minted by the daemon under the reserved
  `dira-probe-` prefix and admitted only while its arm is live; every `Store`
  read filters that prefix, and the daemon never spawns the hook child — a
  child it forked would inherit an elevated token and certify the very bug the
  probe exists to catch. See DIRASH-0023.
- `dira onboard` steps never abort the run (`StepOutcome`, not `?`), and its
  detection pass never writes or spawns anything that writes — `--print` must
  leave the filesystem byte-identical. See DIRASH-0029.
- Knowledge-content consent is asked by its own prompt naming what it sends,
  never implied by device linking or billing. Changing what `full` transmits
  changes `KNOWLEDGE_DISCLOSURE` in the same commit. See DIRASH-0030.
- One backoff ladder: `dira_core::sync::Backoff`. Never re-add a local
  seed/double/cap; attempt budgets stay with the caller. See DIRASH-0031.
- A record's `first_commit`/`created_at`/`source_session` are repaired as a
  unit and only ever earlier, with attribution read from the `artifacts` row
  for the introducing commit — never from the session doing the repair. See
  DIRASH-0032.
- Nothing enters `repo_dirs` unless the directory demonstrably belongs to the
  repo it is filed under, and `register_repo_dir` stays I/O-free.
  See DIRASH-0027.
