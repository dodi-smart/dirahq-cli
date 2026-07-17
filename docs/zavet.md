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
  walked commit: frontmatter (`id`, `title`, `status`, `guards`, `supersedes`)
  plus body, upserted by `(repo, id)` with first-sight provenance (the
  introducing commit, its author date, and its attributed session are
  preserved forever); `content_hash` is the git blob oid.
- **Living specs** — `.zavet/specs/<slug>.md` files (flat, dot-prefixed
  templates ignored; the filename stem is the identity), same first-sight
  upsert by `(repo, slug)`. Frontmatter contract (shared with the plugin's
  parser, inline `#` comments allowed on structured lines, never on `title`):
  `title`, `version`, `origin` (`designed` | `session` | `reverse-engineered`),
  `verified` (true only after a human confirms spec matches code),
  `confidence` (`low` | `med` | `high`), `date`, `paths` (git pathspecs the
  spec covers), `decisions` (optional links). Decision links are derived as
  the frontmatter list ∪ every `D-NNNN` reference in the body, canonicalized —
  links live on the spec side only, decisions stay append-only.

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

- `dira zavet status` — activation verdict (and why), capture health.
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
- `dira zavet wiki [topic]` — browse the knowledge base: active + superseded
  decisions with verification badges, the SPECS section (origin + confidence
  badges, `⚠ stale · N commits` vs `✓ current`, covered paths, linked
  decisions), capture counts, and the recent-trailer chronicle; with a topic,
  ranked matches.
- `dira zavet decisions` — captured decisions for the repo.
- `dira zavet enable|disable|reset` — the per-repo override.

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
  refs, guard events, shas (`recordSha` = the git blob oid), and per-repo
  coverage/capture counts — enough for dashboards to show structure, cost,
  and guard telemetry without any prose. `full` adds the consent-gated
  content fields (`bodyMd`, trailer `value`), which are pinned by an explicit
  path allowlist in the contract's no-content invariant.
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
