-- Source-backed specs (spec-driven-development frameworks) and per-commit /
-- per-session spec binding. Contract 1.5.
--
-- A spec unit from OpenSpec, Spec Kit, BMAD or superpowers resolves into the
-- SAME zavet_specs object native specs use (no new noun): slug `<kind>:<id>`,
-- origin `external`, never verified. The source columns are NULL for native
-- specs. TEXT NEVER LANDS HERE: `source_json` is the contract `SpecSource`
-- minus its full-tier fields (requirement/task text, summary, tracker refs) —
-- the text is re-read from the repo at full-tier sync time, exactly like the
-- consent-gated body (D-0001, DIRASH-0030). The unit id, declared root and the
-- framework's own state word live only in `source_json`; the columns here are
-- the ones queried or stamped independently of it.
ALTER TABLE zavet_specs ADD COLUMN source_kind      TEXT;  -- openspec | speckit | bmad | superpowers
ALTER TABLE zavet_specs ADD COLUMN source_state     TEXT;  -- normalised lifecycle state
ALTER TABLE zavet_specs ADD COLUMN state_changed_at TEXT;  -- RFC 3339, stamped when source_state changes
ALTER TABLE zavet_specs ADD COLUMN source_json      TEXT;  -- SpecSource JSON, text-free

CREATE INDEX IF NOT EXISTS idx_zavet_specs_source
    ON zavet_specs (repo, source_kind) WHERE source_kind IS NOT NULL;

-- One row per comma-separated value of a trailer, so "which commits cite this
-- spec" is an exact, indexable match instead of `TRIM(value) = slug`, which
-- silently missed `Spec: a, b`. Only `spec` trailers are split (the sole
-- list-valued key). Populated at trailer insert time by
-- Store::zavet_record_trailers using zavet::spec_trailer_slugs — the same
-- split, so it cannot disagree with the capture-time binding.
CREATE TABLE IF NOT EXISTS zavet_trailer_values (
    sha   TEXT NOT NULL,      -- commit sha
    repo  TEXT,
    key   TEXT NOT NULL,      -- normalized trailer key (lowercase)
    value TEXT NOT NULL,      -- one trimmed, non-empty list item
    PRIMARY KEY (sha, key, value)
);
CREATE INDEX IF NOT EXISTS idx_zavet_trailer_values_lookup
    ON zavet_trailer_values (repo, key, value);

-- Backfill: split every existing `spec` trailer on commas (recursive CTE; the
-- trim set mirrors Rust's str::trim for the ASCII whitespace that can occur in
-- a one-line trailer value).
WITH RECURSIVE split(sha, repo, rest, item) AS (
    SELECT sha, repo, value || ',', ''
      FROM zavet_trailers WHERE key = 'spec'
    UNION ALL
    SELECT sha, repo,
           SUBSTR(rest, INSTR(rest, ',') + 1),
           TRIM(SUBSTR(rest, 1, INSTR(rest, ',') - 1), ' ' || CHAR(9) || CHAR(10) || CHAR(13))
      FROM split WHERE rest <> ''
)
INSERT OR IGNORE INTO zavet_trailer_values (sha, repo, key, value)
SELECT sha, repo, 'spec', item FROM split WHERE item <> '';

-- Session attribution: the spec slug a session works under
-- (`dira start --spec <slug>`). Sessions are derived from the event log, so
-- the column lives on events beside `label` / `note` and rolls up as
-- first-non-null into SessionRollup.spec.
ALTER TABLE events ADD COLUMN spec TEXT;
