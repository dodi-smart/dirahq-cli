-- Zavet living specs: sectioned feature documents captured from
-- .zavet/specs/<slug>.md, per repo. Same LOCAL-ONLY posture as 0002 —
-- `body_md` never rides the attestation wire (content-free by invariant,
-- D-0001), `content_hash` exists from day one for M2 sync idempotency.
-- Staleness (commits touching a spec's paths after its last capture) is NOT
-- materialized: no table stores per-commit paths, so it is computed at query
-- time from git.

-- One row per spec file, upserted by (repo, slug) as commits touch it.
-- First-sight fields (first_commit, created_at, source_session) are preserved
-- on conflict — provenance points at the commit that INTRODUCED the spec.
CREATE TABLE IF NOT EXISTS zavet_specs (
    repo           TEXT NOT NULL,      -- canonical repo ref, e.g. github.com/org/repo
    slug           TEXT NOT NULL,      -- filename stem — the spec's identity
    title          TEXT,
    version        INTEGER NOT NULL DEFAULT 1,  -- frontmatter version (bumped on regeneration)
    origin         TEXT,               -- designed | session | reverse-engineered
    verified       INTEGER,            -- 1/0/NULL; true only after human review
    confidence     TEXT,               -- low | med | high
    date           TEXT,               -- frontmatter date (spec's own claim), verbatim
    path           TEXT NOT NULL,      -- repo-relative file path
    body_md        TEXT,               -- full spec body (LOCAL-ONLY, never on wire)
    content_hash   TEXT,               -- git blob sha at last_commit (M2 idempotency)
    first_commit   TEXT,               -- sha that introduced the file
    last_commit    TEXT,               -- sha that last touched it
    created_at     TEXT,               -- author date of first_commit (RFC 3339)
    updated_at     TEXT,               -- author date of last_commit (RFC 3339)
    source_session TEXT,               -- session attributed to first_commit
    PRIMARY KEY (repo, slug)
);

-- Path globs a spec covers (frontmatter `paths:`) — the staleness domain.
-- Replaced wholesale on each upsert, mirroring zavet_guards.
CREATE TABLE IF NOT EXISTS zavet_spec_paths (
    repo TEXT NOT NULL,
    slug TEXT NOT NULL,
    glob TEXT NOT NULL,
    PRIMARY KEY (repo, slug, glob)
);

-- Decision links, spec side only (specs are living documents, decisions are
-- append-only — the living doc carries the pointers). Derived at capture from
-- the frontmatter `decisions:` list ∪ body D-refs, so replaced wholesale on
-- upsert. No FK to zavet_decisions: a spec may reference a decision captured
-- later (or never); dangling links render as plain ids.
CREATE TABLE IF NOT EXISTS zavet_spec_decisions (
    repo        TEXT NOT NULL,
    slug        TEXT NOT NULL,
    decision_id TEXT NOT NULL,          -- canonical D-NNNN
    PRIMARY KEY (repo, slug, decision_id)
);

CREATE INDEX IF NOT EXISTS idx_zavet_spec_decisions_decision ON zavet_spec_decisions (repo, decision_id);
