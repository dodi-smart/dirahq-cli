-- Knowledge-sync watermarks (M2, issue #30).
--
-- The knowledge sync task selects "changed since cursor" per table. Decisions
-- and specs are UPSERTED (git author dates are non-monotonic under rebase /
-- backfill, so `updated_at` cannot be a watermark) — they get `touched_seq`,
-- a table-local monotonic sequence bumped on every write by the upsert
-- (single-writer daemon + SQLite write lock make MAX+1 race-free). Backfilled
-- from rowid so pre-existing rows sync exactly once. Trailers are insert-only
-- (rowid cursor) and guard events are ULID-keyed (id cursor) — no columns
-- needed there.

ALTER TABLE zavet_decisions ADD COLUMN touched_seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE zavet_specs ADD COLUMN touched_seq INTEGER NOT NULL DEFAULT 0;

UPDATE zavet_decisions SET touched_seq = rowid;
UPDATE zavet_specs SET touched_seq = rowid;

CREATE INDEX IF NOT EXISTS idx_zavet_decisions_touched
    ON zavet_decisions (touched_seq);
CREATE INDEX IF NOT EXISTS idx_zavet_specs_touched
    ON zavet_specs (touched_seq);
