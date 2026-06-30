-- Append-only event log: the single source of truth. All derived state
-- (sessions, intervals, attestations) can be rebuilt by replaying this table.
CREATE TABLE IF NOT EXISTS events (
    id             TEXT PRIMARY KEY,   -- ULID, monotonic
    at             TEXT NOT NULL,      -- RFC 3339 UTC
    session_id     TEXT NOT NULL,
    harness        TEXT NOT NULL,
    kind           TEXT NOT NULL,
    cwd            TEXT,
    project        TEXT,               -- canonical repo ref, e.g. github.com/org/repo
    identity_email TEXT,
    branch         TEXT,               -- session branch (git --abbrev-ref HEAD), if any
    tool           TEXT,
    label          TEXT,
    activity       TEXT,
    -- Free-text human description for a manual session (`dira log`/`invoice`/`start`
    -- `--note`, or the trailing comment). Surfaced on the session rollup and synced
    -- to the cloud.
    note           TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_at ON events (at);
CREATE INDEX IF NOT EXISTS idx_events_session ON events (session_id);
CREATE INDEX IF NOT EXISTS idx_events_project ON events (project);
-- Composite (session_id, at): the retention/compaction sweep and the per-session
-- activity/signal scans filter by session and order by time; the single-column
-- idx_events_session can't serve that ordered range.
CREATE INDEX IF NOT EXISTS idx_events_session_at ON events (session_id, at);

-- Small key/value store for device identity, bearer token, sync cursors, etc.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Token usage captured from harness transcripts. A sibling metric to time,
-- never a billing base. `id` is the transcript message uuid, so capture is
-- idempotent (re-reading a transcript never double-counts a turn).
CREATE TABLE IF NOT EXISTS token_usage (
    id           TEXT PRIMARY KEY,   -- transcript message uuid
    at           TEXT NOT NULL,      -- RFC 3339 UTC
    session_id   TEXT NOT NULL,
    project      TEXT,               -- canonical repo ref, if resolved
    model        TEXT NOT NULL,
    input        INTEGER NOT NULL,
    output       INTEGER NOT NULL,
    cache_read   INTEGER NOT NULL,
    cache_create INTEGER NOT NULL,
    est_cost     REAL                -- bundled-pricing estimate, USD (a label)
);

CREATE INDEX IF NOT EXISTS idx_token_usage_session ON token_usage (session_id);
CREATE INDEX IF NOT EXISTS idx_token_usage_at ON token_usage (at);

-- Git artifacts (commits) captured locally for cloud-side anchoring. Capture is
-- idempotent: `sha` is the primary key, so re-walking `git log` never duplicates
-- a commit. The wire `ArtifactRef.id` is the sha too, keeping cloud ingest
-- idempotent on the same key.
--
-- Rows are shipped in order of their implicit `rowid`; the sync cursor (meta key
-- `artifacts_sync_cursor`) holds the last `rowid` confirmed-accepted by the cloud.
--
-- `patch_id` is `git patch-id --stable`: it survives rebase/amend/cherry-pick (the
-- sha does not), so the cloud can re-anchor a commit whose sha was rewritten out of
-- the remote. Nullable: older rows and commits with no diff (merges) carry NULL.
--
-- `session_change_id`, `touched_paths`, and `blobs` are squash-resilient anchoring
-- signals computed over the session's *cumulative* diff (merge-base(upstream,
-- HEAD)..HEAD), not per individual commit. A squash merge collapses N commits into
-- one whose combined diff matches no per-commit patch-id — but it equals the
-- cumulative diff and its tree keeps the same post-image blob SHAs, so persisting
-- these lets the cloud re-anchor a squashed/rewritten commit. Metadata only (hashes
-- and paths) — never a diff or file content. `touched_paths` is a JSON array of
-- strings; `blobs` a JSON array of {"path","blob"} objects, so one column holds the
-- set without a side table. Nullable: older rows, merges, detached HEAD, or a
-- missing upstream base carry NULL.
CREATE TABLE IF NOT EXISTS artifacts (
    sha               TEXT PRIMARY KEY,   -- commit SHA; doubles as the wire ArtifactRef.id
    repo              TEXT,               -- canonical repo ref, e.g. github.com/org/repo
    git_ref           TEXT,               -- branch at capture time
    kind              TEXT NOT NULL,      -- 'commit' (PRs are future)
    authored_at       TEXT,               -- RFC 3339, commit author date
    author_email      TEXT,               -- commit author email (shipped, for anchoring)
    author_name       TEXT,               -- commit author name (local-only; never shipped)
    source_session    TEXT,               -- session the daemon observed for this repo at capture
    message           TEXT,               -- commit subject (first line)
    additions         INTEGER,
    deletions         INTEGER,
    patch_id          TEXT,               -- `git patch-id --stable`; rebase/amend-stable change id
    session_change_id TEXT,               -- cumulative-diff change id (squash-resilient)
    touched_paths     TEXT,               -- JSON array of paths in the cumulative diff
    blobs             TEXT                -- JSON array of {"path","blob"} post-image blob SHAs
);

CREATE INDEX IF NOT EXISTS idx_artifacts_repo ON artifacts (repo);

-- Per-repo HEAD watermark. The daemon walks `<head_sha>..HEAD` on each poll and
-- advances this to the new HEAD, so only commits made while watching are captured
-- (after a bounded one-time backfill on first sight).
CREATE TABLE IF NOT EXISTS repo_baseline (
    repo     TEXT PRIMARY KEY,
    head_sha TEXT NOT NULL
);

-- Daily per-session rollup (retention/compaction). When raw events age past the
-- retention window AND are already synced (id <= sync cursor), the maintenance
-- task summarizes them here and DELETEs the raw rows, capping the unbounded event
-- log. Reports for ranges older than retention read this table instead of raw
-- events; the totals use the same accounting code reports use, so compaction is
-- lossless for reporting.
--
-- Keyed by (day, session_id): a session spanning midnight contributes one row per
-- UTC day. The rollup is additive — re-running the sweep on the same day
-- accumulates rather than overwrites, so partial compaction is safe.
CREATE TABLE IF NOT EXISTS session_rollup_daily (
    day            TEXT NOT NULL,      -- YYYY-MM-DD (UTC)
    session_id     TEXT NOT NULL,
    project        TEXT,               -- canonical repo ref, if resolved
    human_seconds  INTEGER NOT NULL DEFAULT 0,
    active_seconds INTEGER NOT NULL DEFAULT 0,
    prompts        INTEGER NOT NULL DEFAULT 0,  -- human-signal count
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    est_cost       REAL    NOT NULL DEFAULT 0.0,
    PRIMARY KEY (day, session_id)
);

CREATE INDEX IF NOT EXISTS idx_rollup_day ON session_rollup_daily (day);
CREATE INDEX IF NOT EXISTS idx_rollup_project ON session_rollup_daily (project);
