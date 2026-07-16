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

**Attribution rule (everywhere):** the unique active session for the repo
(`SessionRegistry::session_for_repo`) or NULL — never guessed. Unattributed
evidence still counts and is reported, so costs read as honest lower bounds.

## Query surface

- `dira zavet status` — activation verdict (and why), capture health.
- `dira zavet why <question or D-0042>` — answer "why?" from recorded
  knowledge. A decision id answers directly; free text ("why are we polling
  instead of a filesystem watcher") is searched across titles, slugs, guards,
  bodies, and trailer values — one confident hit answers in full (annotated
  with what it matched), several return ranked matches with excerpts. The
  answer carries the record (title, status, supersedes chain, guards, body),
  linked commits, guard-event history, and the **cost panel**: de-duplicated
  human seconds (same accounting as `dira report`), idle-trimmed agent
  seconds, and tokens per evidencing session, with totals.
- `dira zavet wiki [topic]` — browse the knowledge base: active + superseded
  decisions with verification badges, capture counts, and the recent-trailer
  chronicle; with a topic, ranked matches.
- `dira zavet decisions` — captured decisions for the repo.
- `dira zavet enable|disable|reset` — the per-repo override.

Records with `origin: reverse-engineered` / `verified: false` (the
`/zavet:backfill` output) render as amber *unverified — hypothesis* badges
everywhere — recall never presents reconstructed rationale as fact.

## Privacy & cloud (M2 design note — not implemented)

Decision bodies and trailer values are **local-only**, exactly like commit
messages: the attestation wire contract is content-free by tested invariant
(`wire_contract_carries_no_content_fields`), so zavet data can never ride
`AttestationBatch`. The planned M2 channel is a separate `KnowledgeEnvelope`
on its own endpoint with its own sync cursor and its own consent gate:

- metadata-only by default: decision ids, status, guard globs, trailer keys +
  decision refs, `content_hash` — enough for dashboards to show structure,
  cost, and guard telemetry without any prose;
- full content (`body_md`, trailer values) only behind an explicit opt-in knob;
- idempotent ingestion keyed by `content_hash` + commit sha, so
  wipe-and-resync reproduces cloud state.

M1 keeps that door open: `content_hash` exists from day one, trailers join the
`artifacts` table the cloud already anchors on, and no zavet column shares a
name with the wire denylist tokens.
