# Frontmatter dialect goldens

Shared, executable definition of the frontmatter dialect parsed by BOTH
`bin/zavet`'s awk parsers and `dira-core::zavet` (the named reference
implementation, `cli/core/src/zavet.rs` in dodi-smart/dirahq-cli). The
canonical copy lives in dirahq-cli at `cli/core/testdata/zavet-dialect/`;
this directory is a byte-identical vendored copy guarded by `MANIFEST`
(`git hash-object` per file — run `test/sync-check.sh`).

Both sides execute the same inputs and must produce these exact tables:

| golden | producer (sh) | producer (Rust) |
|---|---|---|
| `expected/decisions-meta.tsv` | `zavet list` | `parse_decision` → `id \t status(default active) \t title(or empty)` |
| `expected/decisions-guards.tsv` | `zavet guards` | one row per guard, **active decisions only** |
| `expected/specs-meta.tsv` | `zavet specs` | `parse_spec` → `slug \t origin \t confidence \t date(or empty) \t title(or empty)` |
| `expected/spec-paths.tsv` | `zavet spec-paths` | one row per path |

Row order = filename sort order (the sh glob and the Rust walker both sort).

## What the cases pin

- defaults: missing/comment-only `status:` → `active`; missing/comment-only
  `origin:` → `reverse-engineered`; missing `confidence:` → `low`
- inline `# comments` strip on structured lines, NEVER on `title`
- quotes strip on structured values and list items
- inline `[a, b]` and block `- item` lists; empty `[]`
- block lists: items at any indentation (space or tab after the dash),
  trailing whitespace trimmed, indented non-item lines tolerated, the first
  non-indented non-item line (e.g. the next key) TERMINATES the list
- an unclosed `---` fence, a missing opening fence, or a missing/blank
  decision `id:` yields nothing
- superseded decisions keep their meta row and lose their guard rows
- dot-prefixed spec files (`.hidden-template.md` here) are templates,
  excluded by FILENAME on both sides — not a parser rule

## Deliberately out of scope (documented divergences)

- **Id canonicalization** is a Rust-only extra (`D-1` → `D-0001`; malformed
  ids reject the document). Fixture ids are always canonical `D-NNNN`.
- **Indented key lines** (`  title: x` outside a list): the Rust splitter
  tolerates them, the awk anchors keys at column 0. Frontmatter keys are
  top-level; don't indent them.
- `version:` and `decisions:` keys are parsed by Rust only; the sh
  projections simply never show them (fixtures include them to prove both
  sides tolerate their presence).
- Body `D-NNNN` auto-linking is capture-side behavior, not frontmatter.

Editing anything here means regenerating goldens on BOTH sides, updating
both MANIFESTs, and keeping the copies byte-identical.
