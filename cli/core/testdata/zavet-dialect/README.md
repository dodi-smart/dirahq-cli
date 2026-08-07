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
| `expected/decision-checks.tsv` | `zavet checks` | one row per check, `id \t label \t command`, **every status** |
| `expected/specs-meta.tsv` | `zavet specs` | `parse_spec` → `slug \t origin \t confidence \t date(or empty) \t title(or empty)` |
| `expected/spec-paths.tsv` | `zavet spec-paths` | one row per path |
| `expected/spec-checks.tsv` | `zavet spec-checks` | one row per check, `slug \t label \t command` |

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
- `checks:` items split on the FIRST ` :: ` into label and command; an item
  with no separator IS the command and doubles as its own label; an item with
  a label and no command verifies nothing and DROPS; a command keeps any
  later `::`; quoting is the escape hatch for a command containing ` #`,
  which the shared decomment rule would otherwise truncate
- checks emit for every status, unlike guards — a check is a claim about how a
  record was verified, not an enforcement surface
- `corrected-by:` is the errata pointer: the record stays `active` and keeps
  its body, and a pointer at a record that does not exist still parses
  (danglingness is a store-side finding, not a parse error)

## The corpus `config`

`config` is the corpus's own `.zavet/config`, and BOTH sides read it: the
plugin's `test/run.sh` copies it into the fixture repo, the Rust walker parses
it in `corpus_config()`. It declares `prefix: ZAVET`, `prefix-aliases: D` and
`id-width: 4`, which is what lets `ZAVET-0018` and every older `D-*` record
resolve side by side — a retired prefix stays in the set forever, because
records are append-only and an id keeps the prefix it was minted under.

Decision filenames are matched by SHAPE (`<PREFIX>-<digits>[-slug].md`, the
prefix uppercase) rather than against the config's prefix set — the directory
is closed, so shape is enough, and requiring uppercase is what keeps a stray
`notes-2024.md` from reading as a record. Free-text scanning is the opposite:
it is restricted to the config's prefix set precisely because prose contains
`UTF-8`, `SHA-256` and `RFC-2119`.

## Deliberately out of scope (documented divergences)

- **Id canonicalization** is a Rust-only extra (`D-1` → `D-0001`; malformed
  ids reject the document). Fixture ids are always canonical.
- **Indented key lines** (`  title: x` outside a list): the Rust splitter
  tolerates them, the awk anchors keys at column 0. Frontmatter keys are
  top-level; don't indent them.
- `version:` and `decisions:` keys are parsed by Rust only; the sh
  projections simply never show them (fixtures include them to prove both
  sides tolerate their presence).
- **Id canonicalization of `corrected-by`** is likewise Rust-only: `D-7`
  becomes `D-0007` on the Rust side, while the sh projection reports the
  frontmatter verbatim. `D-0015` pins that both sides tolerate the short form.
- Body `D-NNNN` auto-linking is capture-side behavior, not frontmatter.

Editing anything here means regenerating goldens on BOTH sides, updating
both MANIFESTs, and keeping the copies byte-identical.

## Cross-repo coupling

zavet's `fixture-sync` CI job diffs `MANIFEST` against the canonical copy on
dirahq-cli's **`develop`** branch, not `main`. That looks backwards at first glance — zavet
releases off `main`, so comparing against dira's `main` seems like the obvious choice — but
it's wrong: zavet has one release channel (see zavet's `CONTRIBUTING.md`), so whatever's on
`main` there ships to every installed user as soon as it merges. dira's dialect changes land
on `develop` first and only reach dira's `main` when dira itself cuts a release — by which
point a zavet release built against dira's stale `main` copy would already be wrong for the
dira version that's actually shipping next. Comparing against `develop` is what keeps zavet's
shipped dialect matched to the dialect the *next* dira release ships, instead of the last one.

Because this file is byte-identical in both repos, it is written from neither repo's point of
view: "here"/"this directory" would be false in one of the two copies. Name the repo instead.
