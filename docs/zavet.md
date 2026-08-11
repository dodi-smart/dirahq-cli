# Zavet — the knowledge module

Zavet is dira's knowledge sibling: dira answers *"where did the time go?"*,
zavet answers *"what did that time produce, and why?"*. The repo layer (the
`.zavet/` directory format, decision records, guard hooks, slash commands)
lives in its own product — the [zavet plugin](https://github.com/dodi-smart/dirahq-zavet).
This repo contributes the optional dira capability: guard-event ingress,
trailer + decision capture during the ordinary git poll, session attribution,
local storage, and the `dira zavet` query surface.

**Each product works without the other.** The plugin is fully functional with
no dira installed (capture, recall, enforcement — just no time correlation);
dira is inert for repos that are not zavet-active.

## Install

`dira zavet install` installs (or updates) the zavet Claude Code plugin by
shelling out to the `claude` CLI — it never hand-edits Claude Code's own
config. `dira init` writes `.claude/settings.json` directly, but that
precedent doesn't transfer here: plugin install is *stateful* (clone a
marketplace, checkout a version into the plugin cache, and update a
registry file that already carries its own schema version), and
hand-writing that state would declare a marketplace that was never actually
cloned and could race a live Claude Code session rewriting the same files.

```
dira zavet install                  install at user scope (the default)
dira zavet install --scope project  or: project, local
dira zavet install --update         already installed: refresh the
                                     marketplace + plugin instead of a no-op
dira zavet install --dry-run        print the exact `claude` invocations
                                     without running them
```

State is always detected first, never assumed: `claude plugin list --json`
(matching `id == "zavet@dirahq"`), falling back to reading Claude Code's own
`installed_plugins.json` only when that fails, and only trusting that
fallback file when its top-level `"version"` field is exactly `2` — a
schema bump is a signal to stop trusting our own parsing, not to guess. A
repeat `dira zavet install` against an already-installed plugin is a
read-only no-op unless `--update` is passed. On success the command reports
the installed version, scope, and install path, plus an **advisory** skew
line comparing this dira build against the plugin's self-reported
`min_dira` (never a hard error — see below) and a reminder to restart
Claude Code to apply the change. `dira zavet status` prints the same
plugin line, so a stale install is visible from the everyday command, not
just from `install` itself.

If `claude` isn't on `PATH`, `dira zavet install` prints the manual
two-command recipe and exits non-zero rather than half-installing anything:

```
/plugin marketplace add dodi-smart/dirahq-zavet
/plugin install zavet@dirahq
```

### The stable integration contract

`dira zavet install` depends on four things staying stable across both
repos. **Changing any of these is a breaking change that needs a
coordinated release:**

1. **Identity** — marketplace `dirahq` + plugin `zavet` compose to the id
   `zavet@dirahq`, the string both `claude plugin list --json` and
   `installed_plugins.json` key on.
2. **Source** — repo slug `dodi-smart/dirahq-zavet`, marketplace manifest at
   `.claude-plugin/marketplace.json`, default branch `main`. A github
   marketplace source records no `ref`, so `claude plugin marketplace add`
   always clones whatever the default branch currently is.
3. **The version probe** — an executable `bin/zavet` at the plugin root
   supporting `version` and `version --json`, the latter emitting
   `{"v":1,"plugin":"zavet","version":…,"emit_schema":1,"min_dira":…}`.
   This is the only machine-readable compatibility signal; dira never gates
   on it — see "Compatibility posture" below — but it must keep parsing.
4. **Guard-event schema v1** — the JSON object shape on stdin of
   `dira zavet emit`, documented above under "The plugin ↔ dira interface".

**Compatibility posture: surface skew, never gate on it.** The skew line is
advisory only. An installed plugin build that predates the `version`
subcommand — or any other failure resolving it (binary missing, non-zero
exit, unparseable output) — degrades to an "unknown" skew line, never an
error: the whole product promise is that dira and zavet each work fully
without the other.

### Cross-harness adapter refresh (repo-scope, gated)

As of zavet **1.3.0** the plugin also writes a set of generated, COMMITTED
cross-harness adapter artifacts into a repo — `.zavet/bin/zavet` (a vendored
copy of the CLI), an `AGENTS.md` marker block, `.grok/rules/zavet.md`,
`.grok/hooks/zavet.json`, and `.zavet/githooks/{commit-msg,pre-commit}` — via
`zavet adapters`. Existing repos get none of it on upgrade, and the vendored
copy goes stale silently on every later plugin update. `dira zavet install`
folds that refresh in as a best-effort tail step, **but only when every one
of these holds**:

1. `cwd` resolves to a git toplevel (`RepoGate::NotGit` otherwise).
2. That toplevel carries a `.zavet/` directory (`RepoGate::NoZavetDir`
   otherwise) — a repo that never adopted zavet has nothing for adapters to
   refresh.
3. The **installed** zavet (via `zavet version --json`, parsed and compared
   as a release triple) is **1.3.0 or newer** — 1.2.0 has no `adapters`
   subcommand at all, and running `adapters --check` against it prints
   general usage/help text and exits 1, the SAME exit code 1.3.0 uses for
   "stale". Exit code alone cannot tell those apart, so this version guard
   runs before `adapters --check` is ever invoked.
4. `zavet adapters --check` (run pinned to the repo root, never dira's own
   cwd) reports the artifacts stale.

Steps 1–3 are deliberately checked by dira itself, before zavet is ever
spawned: `zavet adapters --check` from a non-git directory (e.g. `$HOME`)
prints "not inside a git repository", reports all six artifacts
missing/stale, and exits 1 — indistinguishable by exit code from genuine
staleness. Without dira's own gate, running `dira zavet install` (a
machine-scope command, valid from anywhere) from an unrelated directory
could otherwise write adapter files where none belong.

Every printed adapter line names the repo path it applies to, so a user who
ran `--update` from an unexpected checkout can see which tree was touched.
Pass `--no-adapters` to skip this refresh even when it would otherwise
apply:

```
dira zavet install --update --no-adapters   refresh the plugin, skip adapters
dira zavet install --update --dry-run       plans the adapters invocation too,
                                             printed but never run:
                                             [dry-run] <plugin-root>/bin/zavet adapters   # in <repo>
```

**Git hooks are never installed by dira.** `zavet hooks --check` is run
alongside the adapters check purely to report `githooks: active` or
`githooks: inactive — run \`zavet hooks install\` in <repo>`; dira never runs
`zavet hooks install` itself, because `core.hooksPath` is not zavet's alone
to own — Husky, lefthook, and plain pre-commit all set it too, and zavet
itself refuses to take it over.

**The four-item stable contract above is unchanged.** This is a
version-gated best-effort addition layered on top of it, not a fifth item —
an installed zavet older than 1.3.0 (or a repo with no `.zavet/`, or a
non-git cwd) simply sees a `not checked`/`unknown` line and nothing else
about `dira zavet install` changes.

## Activation

| Layer | Mechanism | Precedence |
|---|---|---|
| Per-repo override | `dira zavet enable\|disable\|reset` (meta table) | highest |
| Global knob | `[modules] zavet = "auto" \| "on" \| "off"` in `config.toml`, or `DIRA_MODULES__ZAVET` | middle |
| Auto probe | in `auto`, active iff `.zavet/` exists at the repo toplevel | lowest |

A committed `.zavet/` therefore lights zavet up for every dira user of a team
repo; individuals can still opt out per repo.

## The plugin ↔ dira interface (guard event schema v1)

The plugin's hooks report guard activity by piping one JSON object to
`dira zavet emit` (stdin), guarded by `command -v dira` and run in the
background — **fire-and-forget**. The shim forwards it over the daemon's Unix
socket with a 500 ms budget and always exits 0; a missing daemon, an older
dira, or a malformed payload can never affect the hooks.

```json
{
  "v": 1,
  "kind": "guard_shown | guard_blocked | guard_complied | guard_overridden | decision_superseded",
  "decision_id": "D-0042",
  "file_path": "src/auth/session.ts",
  "cwd": "/abs/path/inside/repo",
  "ts": "2026-07-15T12:00:00Z"
}
```

Contract rules (daemon side):

- `v`, `kind`, `decision_id`, `cwd` are required; everything else optional.
- Unknown fields are ignored; unknown `kind`s are **stored verbatim** (a
  plugin newer than the daemon degrades to "recorded, filtered at query time").
- `decision_id` must look like `D-<something>`; otherwise the event is dropped.
- A `ts` that is not RFC 3339 degrades to the daemon's receive time.
- The daemon resolves the canonical repo from `cwd` via its own git resolver —
  it never trusts a caller-supplied repo identity.
- Events for repos where zavet is inactive are dropped (debug-logged).

## Capture

No filesystem watcher. The daemon's existing event-driven commit poll, when a
repo is zavet-active, additionally captures inside the same 10 s blocking
budget:

- **Trailers** — one batched `git log --no-walk` over the walked shas; keys
  `Why: Rejected: Constraint: Refs: Supersedes: Spec:` (case-insensitive),
  first `D-NNNN` reference extracted; idempotent by `(sha, seq)`.
- **Decision records** — `.zavet/decisions/*.md` files added/modified by each
  walked commit: frontmatter (`id`, `title`, `status`, `guards`, `supersedes`,
  `checks`, `corrected-by`) plus body, upserted by `(repo, id)` with
  first-sight provenance (the
  introducing commit, its author date, and its attributed session are
  preserved forever); `content_hash` is the git blob oid.
- **Living specs** — `.zavet/specs/<slug>.md` files (flat, dot-prefixed
  templates ignored; the filename stem is the identity), same first-sight
  upsert by `(repo, slug)`. Frontmatter contract (shared with the plugin's
  parser, inline `#` comments allowed on structured lines, never on `title`):
  `title`, `version`, `origin` (`designed` | `session` | `reverse-engineered`),
  `verified` (true only after a human confirms spec matches code),
  `confidence` (`low` | `med` | `high`), `date`, `paths` (git pathspecs the
  spec covers), `decisions` (optional links), `checks`. Decision links are
  derived as the frontmatter list ∪ every `D-NNNN` reference in the body,
  canonicalized — links live on the spec side only, decisions stay
  append-only.

**Checks** (`checks:` on either record type) bind an invariant to the command
that proves it, as `label :: command` — an item with no separator IS the
command. dira displays them and never runs them: a command read out of a repo
must only execute when a human explicitly asks, which is `zavet verify`'s job
in the plugin. Nothing here detects, infers or special-cases a test framework;
the command is opaque, and exit 0 is the whole contract.

**Corrections** (`corrected-by:` on a decision) are the light sibling of
`supersedes`. Supersession replaces a record wholesale, which is too heavy when
one claim inside it turns out wrong; a corrected record stays `active` and
keeps its body, and every recall path leads with the correction instead. The
pointer may dangle — that is a finding for the plugin's `zavet check`, never a
parse error.

**Staleness** is never materialized: no table stores per-commit paths, so it
is computed at query time — `git log <last_commit>..HEAD -- :(glob)<path>…`
counts the commits that touched a spec's declared paths after its last
capture. The shellout runs in the repo directory the daemon last observed
(falling back to the caller's cwd); with neither, staleness reads *unknown*,
never guessed.

**Attribution rule (everywhere):** the unique active session for the repo
(`SessionRegistry::session_for_repo`) or NULL — never guessed. Unattributed
evidence still counts and is reported, so costs read as honest lower bounds.

## Query surface

- `dira zavet status` — activation verdict (and why), capture health, a
  client-side plugin line (installed version, scope, enabled) resolved the
  same way `dira zavet install` detects state — see "Install" above — and,
  when the installed zavet is 1.3.0+, read-only `adapters:`/`githooks:`
  lines from the same gate described in "Cross-harness adapter refresh"
  (never a write — `status` only ever checks). These lines are **cwd-scoped**
  and are suppressed entirely when `--project` is passed: a named remote
  project has no necessary relationship to the directory the process is
  standing in, and reporting this cwd's adapter staleness against it would
  be misleading.
- `dira zavet why <question, D-0042, or spec slug>` — answer "why?" from
  recorded knowledge. A decision id or an exact spec slug answers directly;
  free text ("why are we polling instead of a filesystem watcher") ranks
  decisions AND specs across titles, slugs, guards/paths, bodies, linked
  decision ids, and trailer values — one confident hit (score ≥ 2× the
  runner-up across both pools) answers in full, several return ranked
  matches with excerpts. A decision answer carries the record, linked
  commits, guard-event history, and its covering specs; a spec answer
  carries the document, its origin/confidence/staleness badges, linked
  decisions, and `Spec:`-trailer commits. Both end in the **cost panel**:
  de-duplicated human seconds (same accounting as `dira report`),
  idle-trimmed agent seconds, and tokens per evidencing session, with totals.
- `dira zavet wiki [topic]` — browse the knowledge base: an attention line
  (uncaptured / off-branch / unverified / stale counts, shown only when
  something needs a human), decisions grouped by branch presence, the SPECS
  section (origin + confidence badges, `⚠ stale · N commits` vs `✓ current`,
  path and decision counts), and the recent-trailer chronicle; with a topic,
  ranked matches. Sections cap at ten rows and point at `dira zavet
  decisions` — it is an overview, not the list. `--json` emits the view as
  one object.
- `dira zavet decisions` — captured decisions for the repo, one row each:
  id, title, guard count, guard activity, age. `--guards` spells out the
  globs (wrapped under the title, never run off the edge), `--branch`
  narrows to the checked-out branch, `--json` emits the view.
- `dira zavet enable|disable|reset` — the per-repo override.

### Branch presence (what the list is scoped to)

The store keys knowledge by repo alone — decision ids are minted repo-wide,
and records are append-only — so a decision recorded on another branch keeps
listing forever. Both list views therefore report what the **working tree**
says about each record, without ever removing a row:

| group | meaning | remedy |
|---|---|---|
| `ACTIVE` / `SUPERSEDED` | the record's file is in `HEAD`'s tree | — |
| `OFF BRANCH` | captured, but its file is not in this tree | none needed; it governs another branch |
| `UNCAPTURED · uncommitted` | on disk, not in `HEAD` | commit it |
| `UNCAPTURED · awaiting sweep` | committed, but the daemon has not walked that commit yet | wait for the next sweep (30 s active) |

`UNCAPTURED` exists because **capture reads git objects, never the working
tree** (`.zavet/config` is the single exception). A record written by
`/zavet:decide` and not yet committed is invisible to every query, and
without this section it is invisible *silently* — which reads as dira having
lost it.

Presence needs a working directory to resolve. With `--project <repo>` from
somewhere else, or on a repo the daemon has never seen a session in, presence
is **unknown** and the groups collapse to one plain list — it is never
guessed, the same rule spec staleness follows.

Records with `origin: reverse-engineered` / `verified: false` (the
`/zavet:backfill` output) render as amber *unverified — hypothesis* badges
everywhere — recall never presents reconstructed rationale as fact.

## Privacy & cloud (M2 — the knowledge channel)

Zavet data never rides `AttestationBatch`: the attestation wire stays
content-free by tested invariant (`wire_contract_carries_no_content_fields`).
M2 ships the separate **`KnowledgeEnvelope`** — same JCS/Ed25519 framing, its
own endpoint (`POST /api/v1/knowledge`), its own four sync cursors, and its
own **double consent gate**:

- **Producer knob** — `[sync] knowledge = "off" | "metadata" | "full"` in
  `config.toml` (or `DIRA_SYNC__KNOWLEDGE`). Default **off**: zavet works
  fully on the machine, nothing leaves it. `metadata` ships decision ids,
  slugs, titles, status, guard globs, spec paths, trailer keys + decision
  refs, guard events, shas (`recordSha` = the git blob oid), check LABELS,
  `correctedBy`, and per-repo coverage/capture counts — enough for dashboards
  to show structure, cost, and guard telemetry without any prose. The coverage
  surface is active decisions' guard globs ∪ **every** spec's paths:
  `verified` records a human review of whether a spec matches the code, which
  is a different question from whether the code is documented, and decisions
  have always counted while unverified (D-0017). `full` adds the consent-gated
  content fields (`bodyMd`, trailer `value`, and a check's `command`), which
  are pinned by an explicit path allowlist in the contract's no-content
  invariant. A check splits across that boundary deliberately: the label names
  an invariant the way a title names a record, while the command is a line of
  the repo's own build configuration and can name internal tooling, hosts and
  paths nobody agreed to publish by turning sync on.
- **Workspace tier** (cloud-side) — the dashboard's ZAVET · KNOWLEDGE SYNC
  setting: `off` refuses the channel (`knowledge_disabled`), `metadata`
  refuses content (`content_not_allowed` — the daemon downgrades the window
  with `strip_content()` and retries once, so sync never wedges), `full`
  accepts everything. Content is stored only when BOTH ends said `full`.

Cursors: decisions and specs advance on a `touched_seq` watermark (bumped on
every upsert — git author dates are non-monotonic under rebase, so
`updated_at` can't be a cursor), trailers on rowid, guard events on their
ULID. All four blank together with the attestation cursors on a `dataEpoch`
change, `dira nuke`, or `dira device resync` — and because ingestion is
idempotent by natural keys + a deterministic `batchId`, wipe-and-resync
reproduces cloud state exactly.
