-- Squash-resilient anchoring signals, computed over the session's *cumulative*
-- diff (merge-base(upstream, HEAD)..HEAD) rather than per individual commit.
--
-- A squash merge collapses N commits into one whose combined diff matches none of
-- the per-commit patch-ids — but it equals the cumulative diff and its tree keeps
-- the same post-image blob SHAs. Persisting these lets the daemon ship them so the
-- cloud can re-anchor a squashed/rewritten commit. All metadata only (hashes and
-- paths) — never a diff or file content. Nullable: older rows, merges, detached
-- HEAD, or a missing upstream base carry NULL.
--
-- `touched_paths` and `blobs` are stored as JSON text (a JSON array of strings, and
-- a JSON array of {"path","blob"} objects respectively) so a single column holds
-- the set without a side table; the batch builder decodes them back to the wire
-- shape.
ALTER TABLE artifacts ADD COLUMN session_change_id TEXT;
ALTER TABLE artifacts ADD COLUMN touched_paths TEXT;
ALTER TABLE artifacts ADD COLUMN blobs TEXT;
