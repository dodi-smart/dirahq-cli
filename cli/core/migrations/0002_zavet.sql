-- Zavet knowledge module: decision records, guard globs, commit trailers, and
-- guard events, captured per repo. Everything here is LOCAL-ONLY in M1; the
-- future cloud channel (KnowledgeEnvelope, M2) ships metadata by default and
-- `content_hash` exists from day one so that sync can be idempotent. `body_md`
-- follows the `artifacts.message`/`author_name` precedent: stored locally,
-- never on the attestation wire (which is content-free by invariant).

-- One row per decision record file (.zavet/decisions/D-NNNN-slug.md), upserted
-- by (repo, id) as commits touch it. First-sight fields (first_commit,
-- created_at, source_session) are preserved on conflict — provenance points at
-- the commit that INTRODUCED the decision, not the last one to touch it.
CREATE TABLE IF NOT EXISTS zavet_decisions (
    repo           TEXT NOT NULL,      -- canonical repo ref, e.g. github.com/org/repo
    id             TEXT NOT NULL,      -- decision id, e.g. D-0042
    slug           TEXT,               -- filename slug after the id
    title          TEXT,
    status         TEXT,               -- active | superseded | ...
    path           TEXT NOT NULL,      -- repo-relative file path
    supersedes     TEXT,               -- decision id this one replaces, if any
    body_md        TEXT,               -- full record body (LOCAL-ONLY, never on wire)
    first_commit   TEXT,               -- sha that introduced the file
    last_commit    TEXT,               -- sha that last touched it
    created_at     TEXT,               -- author date of first_commit (RFC 3339)
    updated_at     TEXT,               -- author date of last_commit (RFC 3339)
    source_session TEXT,               -- session attributed to first_commit
    content_hash   TEXT,               -- git blob sha at last_commit (M2 idempotency)
    origin         TEXT,               -- recorded | reverse-engineered
    verified       INTEGER,            -- 1/0/NULL; reverse-engineered starts 0
    PRIMARY KEY (repo, id)
);

-- Guard globs declared in a decision's frontmatter; replaced wholesale on each
-- upsert so narrowing a guard actually narrows it.
CREATE TABLE IF NOT EXISTS zavet_guards (
    repo        TEXT NOT NULL,
    decision_id TEXT NOT NULL,
    glob        TEXT NOT NULL,
    PRIMARY KEY (repo, decision_id, glob)
);

-- Lore-protocol trailers parsed from commit messages (Why/Rejected/Constraint/
-- Refs/Supersedes/Spec). Keyed by (sha, seq) so re-walking a range is a no-op;
-- joins `artifacts.sha` for the commit's source_session. Values are the
-- author's own one-line trailer text — local-only, like the commit subject.
CREATE TABLE IF NOT EXISTS zavet_trailers (
    sha         TEXT NOT NULL,         -- commit sha
    repo        TEXT,
    key         TEXT NOT NULL,         -- normalized trailer key (lowercase)
    value       TEXT NOT NULL,
    decision_id TEXT,                  -- first D-NNNN referenced in value, if any
    seq         INTEGER NOT NULL,      -- trailer order within the commit message
    PRIMARY KEY (sha, seq)
);

CREATE INDEX IF NOT EXISTS idx_zavet_trailers_decision ON zavet_trailers (repo, decision_id);

-- Guard events emitted by the zavet plugin's hooks (schema v1 over
-- `dira zavet emit`): the regression-prevention telemetry. `kind` is stored
-- verbatim so unknown future kinds survive a daemon older than the plugin.
-- `session_id` is attributed at ingest (unique-active-or-NULL, never guessed).
CREATE TABLE IF NOT EXISTS zavet_guard_events (
    id          TEXT PRIMARY KEY,      -- ULID
    at          TEXT NOT NULL,         -- RFC 3339
    repo        TEXT,
    decision_id TEXT NOT NULL,
    kind        TEXT NOT NULL,         -- guard_shown | guard_blocked | guard_complied | ...
    file_path   TEXT,
    session_id  TEXT
);

CREATE INDEX IF NOT EXISTS idx_zavet_guard_events_decision ON zavet_guard_events (repo, decision_id);
CREATE INDEX IF NOT EXISTS idx_zavet_guard_events_at ON zavet_guard_events (at);
