-- Contract 2.0 local state (DIRASH-0052).
--
-- Session facts the event log does not carry, counted at capture time from the
-- hook's tool input, which is never stored: lines the agent's edit/write tools
-- added and removed, context compactions, and the files its tools touched, kept
-- only as a hash of the path so they can be counted distinct. Counts and hashes
-- only; never a path, a line or any content (D-0001).
CREATE TABLE IF NOT EXISTS session_facts (
    session_id    TEXT PRIMARY KEY,
    lines_added   INTEGER NOT NULL DEFAULT 0,
    lines_removed INTEGER NOT NULL DEFAULT 0,
    compactions   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS session_paths (
    session_id TEXT NOT NULL,
    path_hash  TEXT NOT NULL,
    PRIMARY KEY (session_id, path_hash)
) WITHOUT ROWID;

-- Day rollups: what this device has had acknowledged, per UTC day and repo
-- (`repo` '' = none), so each day can be re-sent as a `DayRollup` the cloud
-- reconciles against. Keyed by the wire ids, so a re-sent interval, rollup or
-- token row replaces itself instead of counting twice.
CREATE TABLE IF NOT EXISTS day_intervals (
    id            TEXT PRIMARY KEY,
    day           TEXT NOT NULL,
    repo          TEXT NOT NULL,
    human_seconds INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS day_intervals_day ON day_intervals (day, repo);

CREATE TABLE IF NOT EXISTS day_sessions (
    session_id         TEXT PRIMARY KEY,
    day                TEXT NOT NULL,
    repo               TEXT NOT NULL,
    agent_wall_seconds INTEGER NOT NULL,
    prompts            INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS day_sessions_day ON day_sessions (day, repo);

CREATE TABLE IF NOT EXISTS day_tokens (
    id           TEXT PRIMARY KEY,
    day          TEXT NOT NULL,
    repo         TEXT NOT NULL,
    model        TEXT NOT NULL,
    input        INTEGER NOT NULL,
    output       INTEGER NOT NULL,
    cache_read   INTEGER NOT NULL,
    cache_create INTEGER NOT NULL,
    est_cost_usd REAL
);
CREATE INDEX IF NOT EXISTS day_tokens_day ON day_tokens (day, repo);

CREATE TABLE IF NOT EXISTS day_commits (
    sha  TEXT NOT NULL,
    repo TEXT NOT NULL,
    day  TEXT NOT NULL,
    PRIMARY KEY (sha, repo)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS day_commits_day ON day_commits (day, repo);

-- Which days changed since they were last sent, and the revision the next send
-- carries.
CREATE TABLE IF NOT EXISTS day_rollup_state (
    day      TEXT NOT NULL,
    repo     TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0,
    dirty    INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (day, repo)
) WITHOUT ROWID;
